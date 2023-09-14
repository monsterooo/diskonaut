use ::std::collections::{HashMap, VecDeque};
use ::std::ffi::OsString;
use ::std::fs::Metadata;
use ::std::path::PathBuf;

use ::filesize::PathExt;

// 文件或目录集合枚举
#[derive(Debug, Clone)]
pub enum FileOrFolder {
    Folder(Folder),
    File(File),
}

impl FileOrFolder {
    pub fn size(&self) -> u128 {
        match self {
            FileOrFolder::Folder(folder) => folder.size,
            FileOrFolder::File(file) => file.size,
        }
    }
}

// 文件结构
#[derive(Debug, Clone)]
pub struct File {
    pub name: OsString, // 文件名称
    pub size: u128, // 文件大小
}

// 目录结构
#[derive(Debug, Clone)]
pub struct Folder {
    pub name: OsString, // 目录名称
    pub contents: HashMap<OsString, FileOrFolder>, // 目录内容
    pub size: u128, // 目录大小
    pub num_descendants: u64, // 内容数量
}

// OsString 转 Folder 方法
impl From<OsString> for Folder {
    fn from(name: OsString) -> Self {
        Folder {
            name,
            contents: HashMap::new(),
            size: 0,
            num_descendants: 0,
        }
    }
}
impl Folder {
    pub fn new(path: &PathBuf) -> Self {
        // 拿到目录名称
        let base_folder_name = path.iter().last().expect("could not get path base name");
        Self {
            name: base_folder_name.to_os_string(),
            contents: HashMap::new(),
            size: 0,
            num_descendants: 0,
        }
    }

    // 添加目录或文件到当前目录下
    pub fn add_entry(
        &mut self,
        entry_metadata: &Metadata,
        relative_path: PathBuf,
        show_apparent_size: bool,
    ) {
        // apparent_size (named after the flag of the same name in 'du')
        // means "show the file size, rather than the actual space it takes on disk"
        // these may differ (for example) in filesystems that use compression
        // 添加目录
        if entry_metadata.is_dir() {
            self.add_folder(relative_path);
        } else {
            // 添加文件
            let size = if show_apparent_size {
                entry_metadata.len() as u128
            } else {
                relative_path
                    .size_on_disk_fast(&entry_metadata)
                    .unwrap_or(entry_metadata.len()) as u128
            };
            self.add_file(relative_path, size);
        }
    }

    // 添加目录
    pub fn add_folder(&mut self, path: PathBuf) {
        // 获取路径长度 /a/b => 2
        let path_length = path.components().count();
        if path_length == 0 {
            return;
        }
        if path_length > 1 {
            let name = path
                .iter()
                .next()
                .expect("could not get next path element for folder")
                .to_os_string();
            let path_entry = self
                .contents
                .entry(name.clone())
                .or_insert(FileOrFolder::Folder(Folder::from(name)));
            self.num_descendants += 1;
            match path_entry {
                FileOrFolder::Folder(folder) => folder.add_folder(path.iter().skip(1).collect()),
                _ => unreachable!("got a file in the middle of a path"),
            };
        } else {
            let name = path
                .iter()
                .next()
                .expect("could not get next path element for file")
                .to_os_string();
            self.num_descendants += 1;
            self.contents
                .insert(name.clone(), FileOrFolder::Folder(Folder::from(name)));
        }
    }
    pub fn add_file(&mut self, path: PathBuf, size: u128) {
        let path_length = path.components().count();
        if path_length == 0 {
            return;
        }
        if path_length > 1 {
            let name = path
                .iter()
                .next()
                .expect("could not get next path element for folder")
                .to_os_string();
            let path_entry = self
                .contents
                .entry(name.clone())
                .or_insert(FileOrFolder::Folder(Folder::from(name)));
            self.size += size;
            self.num_descendants += 1;
            match path_entry {
                FileOrFolder::Folder(folder) => {
                    folder.add_file(path.iter().skip(1).collect(), size);
                }
                _ => unreachable!("got a file in the middle of a path"),
            };
        } else {
            let name = path
                .iter()
                .next()
                .expect("could not get next path element for file")
                .to_os_string();
            self.size += size;
            self.num_descendants += 1;
            self.contents
                .insert(name.clone(), FileOrFolder::File(File { name, size }));
        }
    }
    pub fn path(&self, mut folder_names: Vec<OsString>) -> Option<&FileOrFolder> {
        let next_folder_name = folder_names.remove(0);
        let next_in_path = &self.contents.get(&next_folder_name)?;
        if folder_names.is_empty() {
            Some(next_in_path)
        } else if let FileOrFolder::Folder(next_folder) = next_in_path {
            next_folder.path(folder_names)
        } else {
            Some(next_in_path)
        }
    }
    pub fn delete_path(&mut self, folder_names: &[OsString]) {
        // TODO: there are some needless allocations here, this is not terrible since
        // the deletion itself takes an order of magnitude longer, but it can be nice
        // to reduce them
        let mut folders_to_traverse: VecDeque<OsString> = VecDeque::from(folder_names.to_owned());
        if folder_names.len() == 1 {
            let name = folder_names
                .last()
                .expect("could not find last item in path");
            let removed_size = &self
                .contents
                .get(name)
                .expect("could not find folder")
                .size();
            let removed_descendents = match &self.contents.get(name).expect("could not find folder")
            {
                FileOrFolder::Folder(folder) => folder.num_descendants,
                FileOrFolder::File(_file) => 1,
            };
            self.size -= removed_size;
            self.num_descendants -= removed_descendents;
            self.contents.remove(name);
        } else {
            let (removed_size, removed_descendents) = {
                let item_to_remove = self
                    .path(Vec::from(folders_to_traverse.clone()))
                    .expect("could not find item to delete");
                let removed_size = item_to_remove.size();
                let removed_descendents = match item_to_remove {
                    FileOrFolder::Folder(folder) => folder.num_descendants,
                    FileOrFolder::File(_file) => 1,
                };
                (removed_size, removed_descendents)
            };
            let next_name = folders_to_traverse
                .pop_front()
                .expect("could not find next path folder");
            let next_item = &mut self
                .contents
                .get_mut(&next_name)
                .expect("could not find folder in path");
            match next_item {
                FileOrFolder::Folder(folder) => {
                    self.size -= removed_size;
                    self.num_descendants -= removed_descendents;
                    folder.delete_path(&Vec::from(folders_to_traverse));
                }
                FileOrFolder::File(_) => {
                    panic!("got a file in the middle of a path");
                }
            }
        }
    }
}
