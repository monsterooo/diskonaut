#[cfg(test)]
mod tests;

mod app;
mod input;
mod messages;
mod os;
mod state;
mod ui;

use ::failure;
use ::jwalk::Parallelism::{RayonDefaultPool, Serial};
use ::jwalk::WalkDir;
use ::std::env;
use ::std::io;
use ::std::path::PathBuf;
use ::std::process;
use ::std::sync::atomic::{AtomicBool, Ordering};
use ::std::sync::mpsc;
use ::std::sync::mpsc::{Receiver, SyncSender};
use ::std::sync::Arc;
use ::std::thread::park_timeout;
use ::std::{thread, time};
use ::structopt::StructOpt;

use ::tui::backend::Backend;
use crossterm::event::KeyModifiers;
use crossterm::event::{Event as BackEvent, KeyCode, KeyEvent};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tui::backend::CrosstermBackend;

use app::{App, UiMode};
use input::TerminalEvents;
use messages::{handle_events, Event, Instruction};

#[cfg(not(test))]
const SHOULD_SHOW_LOADING_ANIMATION: bool = true;
#[cfg(test)]
const SHOULD_SHOW_LOADING_ANIMATION: bool = false;
#[cfg(not(test))]
const SHOULD_HANDLE_WIN_CHANGE: bool = true;
#[cfg(test)]
const SHOULD_HANDLE_WIN_CHANGE: bool = false;
#[cfg(not(test))]
const SHOULD_SCAN_HD_FILES_IN_MULTIPLE_THREADS: bool = true;
#[cfg(test)]
const SHOULD_SCAN_HD_FILES_IN_MULTIPLE_THREADS: bool = false;

#[derive(StructOpt, Debug)]
#[structopt(name = "diskonaut")]
pub struct Opt {
    #[structopt(name = "folder", parse(from_os_str))]
    /// The folder to scan
    folder: Option<PathBuf>,
    #[structopt(short, long)]
    /// Show file sizes rather than their block usage on disk
    apparent_size: bool,
    #[structopt(short, long)]
    /// Don't ask for confirmation before deleting
    disable_delete_confirmation: bool,
}

fn main() {
    if let Err(err) = try_main() {
        println!("Error: {}", err);
        process::exit(2);
    }
}
fn get_stdout() -> io::Result<io::Stdout> {
    Ok(io::stdout())
}

fn try_main() -> Result<(), failure::Error> {
    let opts = Opt::from_args();

    match get_stdout() {
        Ok(stdout) => {
            enable_raw_mode()?;
            let terminal_backend = CrosstermBackend::new(stdout);
            // 读取终端的数据
            let terminal_events = TerminalEvents {};
            // 获取扫描的目录，如果未传递则使用当前目录
            let folder = match opts.folder {
                Some(folder) => folder,
                None => env::current_dir()?,
            };
            // 如果不是目录则退出程序
            if !folder.as_path().is_dir() {
                failure::bail!("Folder '{}' does not exist", folder.to_string_lossy())
            }
            start(
                terminal_backend,
                Box::new(terminal_events),
                folder,
                opts.apparent_size,
                opts.disable_delete_confirmation,
            );
        }
        Err(_) => failure::bail!("Failed to get stdout: are you trying to pipe 'diskonaut'?"),
    }
    disable_raw_mode()?;
    Ok(())
}

pub fn start<B>(
    terminal_backend: B,
    terminal_events: Box<dyn Iterator<Item = BackEvent> + Send>,
    path: PathBuf,
    show_apparent_size: bool,
    disable_delete_confirmation: bool,
) where
    B: Backend + Send + 'static,
{
    let mut active_threads = vec![];

    // 时间同时只处理一个
    let (event_sender, event_receiver): (SyncSender<Event>, Receiver<Event>) =
        mpsc::sync_channel(1);
    // 指令同时最多处理100个
    let (instruction_sender, instruction_receiver): (
        SyncSender<Instruction>,
        Receiver<Instruction>,
    ) = mpsc::sync_channel(100);

    let running = Arc::new(AtomicBool::new(true));
    let loaded = Arc::new(AtomicBool::new(false));

    // 主要处理文件删除、退出事件
    active_threads.push(
        thread::Builder::new()
            .name("event_executer".to_string())
            .spawn({
                let instruction_sender = instruction_sender.clone();
                || handle_events(event_receiver, instruction_sender)
            })
            .unwrap(),
    );

    active_threads.push(
        thread::Builder::new()
            .name("stdin_handler".to_string())
            .spawn({
                let instruction_sender = instruction_sender.clone();
                let running = running.clone();
                move || {
                    // 阻塞线程，读取终端输入数据
                    for evt in terminal_events {
                        // 移动大小
                        if let BackEvent::Resize(_x, _y) = evt {
                            if SHOULD_HANDLE_WIN_CHANGE {
                                let _ = instruction_sender.send(Instruction::ResetUiMode);
                                let _ = instruction_sender.send(Instruction::Render);
                            }
                            continue;
                        }
                        
                        // 按下 y q c
                        if let BackEvent::Key(KeyEvent {
                            code: KeyCode::Char('y'),
                            modifiers: KeyModifiers::NONE,
                        })
                        | BackEvent::Key(KeyEvent {
                            code: KeyCode::Char('q'),
                            modifiers: KeyModifiers::NONE,
                        })
                        | BackEvent::Key(KeyEvent {
                            code: KeyCode::Char('c'),
                            modifiers: KeyModifiers::CONTROL,
                        }) = evt
                        {
                            // not ideal, but works in a pinch
                            // 向通道发送键盘按下事件
                            let _ = instruction_sender.send(Instruction::Keypress(evt));
                            // 阻塞线程100毫秒
                            park_timeout(time::Duration::from_millis(100));
                            // if we don't wait, the app won't have time to quit
                            if !running.load(Ordering::Acquire) {
                                // sometimes ctrl-c doesn't shut down the app
                                // (eg. dismissing an error message)
                                // in order not to be aware of those particularities
                                // we check "running"
                                break;
                            }
                        } else if instruction_sender.send(Instruction::Keypress(evt)).is_err() {
                            break;
                        }
                    }
                }
            })
            .unwrap(),
    );

    // 扫描文件
    active_threads.push(
        thread::Builder::new()
            .name("hd_scanner".to_string())
            .spawn({
                let path = path.clone();
                let instruction_sender = instruction_sender.clone();
                let loaded = loaded.clone();
                move || {
                    'scanning: for entry in WalkDir::new(&path)
                        .parallelism(if SHOULD_SCAN_HD_FILES_IN_MULTIPLE_THREADS {
                            RayonDefaultPool
                        } else {
                            Serial
                        })
                        .skip_hidden(false)
                        .follow_links(false)
                        .into_iter()
                    {
                        let instruction_sent = match entry {
                            Ok(entry) => match entry.metadata() {
                                Ok(file_metadata) => {
                                    let entry_path = entry.path();
                                    instruction_sender.send(Instruction::AddEntryToBaseFolder((
                                        file_metadata,
                                        entry_path,
                                    )))
                                }
                                Err(_) => {
                                    instruction_sender.send(Instruction::IncrementFailedToRead)
                                }
                            },
                            Err(_) => instruction_sender.send(Instruction::IncrementFailedToRead),
                        };
                        if instruction_sent.is_err() {
                            // if we fail to send an instruction here, this likely means the program has
                            // ended and we need to break this loop as well in order not to hang
                            break 'scanning;
                        };
                    }
                    let _ = instruction_sender.send(Instruction::StartUi);
                    loaded.store(true, Ordering::Release);
                }
            })
            .unwrap(),
    );

    // 更新界面线程
    if SHOULD_SHOW_LOADING_ANIMATION {
        active_threads.push(
            thread::Builder::new()
                .name("loading_loop".to_string())
                .spawn({
                    let instruction_sender = instruction_sender.clone();
                    let running = running.clone();
                    move || {
                        while running.load(Ordering::Acquire) && !loaded.load(Ordering::Acquire) {
                            let _ =
                                instruction_sender.send(Instruction::ToggleScanningVisualIndicator);
                            // 界面更新入口
                            let _ = instruction_sender.send(Instruction::RenderAndUpdateBoard);
                            park_timeout(time::Duration::from_millis(100));
                        }
                    }
                })
                .unwrap(),
        );
    }

    let mut app = App::new(
        terminal_backend,
        path,
        event_sender,
        show_apparent_size,
        disable_delete_confirmation,
    );
    app.start(instruction_receiver);
    eprintln!("abc~~~");
    running.store(false, Ordering::Release);

    for thread_handler in active_threads {
        thread_handler.join().unwrap();
    }
}
