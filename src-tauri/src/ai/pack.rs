use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use memmap2::Mmap;
use parking_lot::{Mutex, RwLock};

use crate::app::error::{AppError, AppResult};

const MAGIC: &[u8; 8] = b"ARTBFTR1";
const VERSION: u32 = 1;
const HEADER_LEN: u64 = 32;
/// 累积这么多新字节就同步一次特征包。
///
/// 逐条 `sync_all` 会让批量生成向量变成逐条 fsync；完全不同步则掉电后包头记录的
/// 偏移可能落在文件末尾之外。取一个字节阈值，剩下的尾部由打开时的对齐检查兜底。
const SYNC_INTERVAL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureKind {
    Embedding,
    Pose,
}

impl FeatureKind {
    fn code(self) -> u8 {
        match self {
            Self::Embedding => 1,
            Self::Pose => 2,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Embedding => "embedding",
            Self::Pose => "pose",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureDtype {
    F16,
    F32,
}

impl FeatureDtype {
    fn code(self) -> u8 {
        match self {
            Self::F16 => 1,
            Self::F32 => 2,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::F16 => "f16",
            Self::F32 => "f32",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeatureLocation {
    pub pack_name: String,
    pub offset: u64,
    pub length: u32,
}

#[derive(Debug, Clone)]
struct PackHeader {
    kind: FeatureKind,
    dtype: FeatureDtype,
    dim: u32,
    record_len: u32,
}

/// 追加式特征包。写入串行；读取按偏移定位。向量包不做整体常驻。
pub struct FeatureStore {
    root: RwLock<PathBuf>,
    writers: Mutex<HashMap<String, PackWriter>>,
}

/// 单个包的写句柄与自上次落盘以来追加的字节数。
struct PackWriter {
    file: File,
    unsynced: u64,
}

impl Default for FeatureStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FeatureStore {
    pub fn new() -> Self {
        Self {
            root: RwLock::new(PathBuf::new()),
            writers: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_root(&self, root: PathBuf) {
        let mut current = self.root.write();
        if *current == root {
            return;
        }
        // 换库前把旧包的尾巴落盘：写句柄马上就丢了。
        self.sync_writers();
        *current = root;
        self.writers.lock().clear();
    }

    pub fn clear(&self) {
        self.sync_writers();
        *self.root.write() = PathBuf::new();
        self.writers.lock().clear();
    }

    /// 把还没有落盘的追加写同步到磁盘；失败只记日志，不打断调用方。
    pub fn sync_writers(&self) {
        let mut writers = self.writers.lock();
        for (name, writer) in writers.iter_mut() {
            if writer.unsynced == 0 {
                continue;
            }
            if let Err(error) = writer.file.flush().and_then(|()| writer.file.sync_all()) {
                log::warn!("特征包 {name} 落盘失败：{error}");
                continue;
            }
            writer.unsynced = 0;
        }
    }

    fn root(&self) -> AppResult<PathBuf> {
        let root = self.root.read().clone();
        if root.as_os_str().is_empty() {
            return Err(AppError::message("尚未打开图库"));
        }
        Ok(root)
    }

    fn file_path(&self, pack_name: &str) -> AppResult<PathBuf> {
        Ok(self.root()?.join(pack_name))
    }

    /// 追加一条记录；返回数据库需要保存的指针。
    pub fn append(
        &self,
        kind: FeatureKind,
        model: &str,
        dim: u32,
        dtype: FeatureDtype,
        bytes: &[u8],
    ) -> AppResult<FeatureLocation> {
        if bytes.is_empty() || dim == 0 {
            return Err(AppError::message("特征数据为空"));
        }
        let expected = expected_record_len(dtype, dim);
        if bytes.len() != expected {
            return Err(AppError::message(format!(
                "特征长度 {} 与 dim={dim} / {:?} 不匹配",
                bytes.len(),
                dtype
            )));
        }

        let pack_name = pack_name(kind, model, dim, dtype);
        let root = self.root()?;
        std::fs::create_dir_all(&root)?;
        let path = root.join(&pack_name);

        let mut writers = self.writers.lock();
        if !writers.contains_key(&pack_name) {
            let file = open_writer(&path, kind, dtype, dim, expected)?;
            writers.insert(pack_name.clone(), PackWriter { file, unsynced: 0 });
        }
        let writer = writers.get_mut(&pack_name).expect("writer inserted");
        let offset = writer.file.metadata()?.len();
        writer.file.write_all(bytes)?;
        writer.file.flush()?;
        writer.unsynced += bytes.len() as u64;
        if writer.unsynced >= SYNC_INTERVAL_BYTES {
            writer.file.sync_all()?;
            writer.unsynced = 0;
        }

        Ok(FeatureLocation {
            pack_name,
            offset,
            length: bytes.len() as u32,
        })
    }

    pub fn read(&self, pack_name: &str, offset: u64, length: u32) -> AppResult<Vec<u8>> {
        let path = self.file_path(pack_name)?;
        let mut file = File::open(&path)?;
        let header = read_header(&mut file, &path)?;
        let file_len = file.metadata()?.len();
        let end = offset
            .checked_add(u64::from(length))
            .ok_or_else(|| AppError::message("特征指针越界"))?;
        if offset < HEADER_LEN || length == 0 || end > file_len {
            return Err(AppError::message("特征指针越界"));
        }
        if length != header.record_len {
            return Err(AppError::message("特征记录长度与包头不一致"));
        }
        let mut bytes = vec![0_u8; length as usize];
        file.seek(std::io::SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    pub fn mmap(&self, pack_name: &str) -> AppResult<Arc<Mmap>> {
        let path = self.file_path(pack_name)?;
        let mut file = File::open(&path)?;
        read_header(&mut file, &path)?;
        let map = unsafe { Mmap::map(&file) }
            .map_err(|error| AppError::message(format!("无法映射特征包：{error}")))?;
        Ok(Arc::new(map))
    }
}

impl Drop for FeatureStore {
    fn drop(&mut self) {
        self.sync_writers();
    }
}

/// 从映射好的包里切出一条记录；越界返回 `None`。
///
/// 批量扫描（embedding 均值/检索、pose 相似度）共用：mmap 一次，逐条切片，不重复开文件。
pub(crate) fn record_slice(map: &[u8], offset: u64, length: usize) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(length)?;
    map.get(start..end)
}

fn expected_record_len(dtype: FeatureDtype, dim: u32) -> usize {
    match dtype {
        FeatureDtype::F16 => dim as usize * 2,
        FeatureDtype::F32 => dim as usize * 4,
    }
}

fn open_writer(
    path: &Path,
    kind: FeatureKind,
    dtype: FeatureDtype,
    dim: u32,
    record_len: usize,
) -> AppResult<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(path)?;
    let size = file.metadata()?.len();
    if size == 0 {
        write_header(&mut file, kind, dtype, dim, record_len as u32)?;
    } else {
        let header = read_header(&mut file, path)?;
        if header.kind != kind || header.dtype != dtype || header.dim != dim {
            return Err(AppError::message(format!(
                "特征包 {} 的格式与当前模型不一致",
                path.display()
            )));
        }
        // 掉电可能留下半条记录：截到整条边界，免得后续读到的指针落在半个记录上。
        let body = size.saturating_sub(HEADER_LEN);
        let remainder = body % record_len as u64;
        if remainder != 0 {
            let trimmed = size - remainder;
            log::warn!(
                "特征包 {} 末尾有 {} 字节的半条记录，截断到 {trimmed} 字节",
                path.display(),
                remainder
            );
            // 追加模式打开的文件只有 FILE_APPEND_DATA，不能改长度；另开一个写句柄截断。
            OpenOptions::new()
                .write(true)
                .open(path)?
                .set_len(trimmed)?;
        }
    }
    Ok(file)
}

fn write_header(
    file: &mut File,
    kind: FeatureKind,
    dtype: FeatureDtype,
    dim: u32,
    record_len: u32,
) -> AppResult<()> {
    let mut header = [0_u8; HEADER_LEN as usize];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12] = kind.code();
    header[13] = dtype.code();
    header[16..20].copy_from_slice(&dim.to_le_bytes());
    header[20..24].copy_from_slice(&record_len.to_le_bytes());
    header[24..32].copy_from_slice(&chrono::Utc::now().timestamp_millis().to_le_bytes());
    file.write_all(&header)?;
    file.flush()?;
    Ok(())
}

fn read_header(file: &mut File, path: &Path) -> AppResult<PackHeader> {
    let size = file.metadata()?.len();
    if size < HEADER_LEN {
        return Err(AppError::message(format!(
            "特征包 {} 头部不完整",
            path.display()
        )));
    }
    let mut header = [0_u8; HEADER_LEN as usize];
    file.seek(std::io::SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;
    if &header[..8] != MAGIC {
        return Err(AppError::message(format!(
            "特征包 {} 格式不匹配",
            path.display()
        )));
    }
    let version = u32::from_le_bytes(header[8..12].try_into().expect("version"));
    if version != VERSION {
        return Err(AppError::message(format!(
            "特征包 {} 版本 {version} 不受支持",
            path.display()
        )));
    }
    let kind = match header[12] {
        1 => FeatureKind::Embedding,
        2 => FeatureKind::Pose,
        other => {
            return Err(AppError::message(format!(
                "特征包 {} 类型 {other} 不受支持",
                path.display()
            )));
        }
    };
    let dtype = match header[13] {
        1 => FeatureDtype::F16,
        2 => FeatureDtype::F32,
        other => {
            return Err(AppError::message(format!(
                "特征包 {} 数据类型 {other} 不受支持",
                path.display()
            )));
        }
    };
    let dim = u32::from_le_bytes(header[16..20].try_into().expect("dim"));
    let record_len = u32::from_le_bytes(header[20..24].try_into().expect("record length"));
    if dim == 0 || record_len as usize != expected_record_len(dtype, dim) {
        return Err(AppError::message(format!(
            "特征包 {} 的维度信息无效",
            path.display()
        )));
    }
    Ok(PackHeader {
        kind,
        dtype,
        dim,
        record_len,
    })
}

fn pack_name(kind: FeatureKind, model: &str, dim: u32, dtype: FeatureDtype) -> String {
    let mut slug = String::new();
    for ch in model.chars().take(48) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let digest = blake3::hash(model.as_bytes()).to_hex();
    let hash = &digest[..8];
    format!(
        "{}-{}-{}-{}-{}.pack",
        kind.name(),
        if slug.is_empty() { "model" } else { slug },
        dim,
        dtype.name(),
        hash
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_reads_records() {
        let temp = tempfile::tempdir().unwrap();
        let store = FeatureStore::new();
        store.set_root(temp.path().to_path_buf());

        let first = store
            .append(
                FeatureKind::Embedding,
                "siglip2-giant-opt-patch16-384",
                4,
                FeatureDtype::F16,
                &[0_u8; 8],
            )
            .unwrap();
        let second = store
            .append(
                FeatureKind::Embedding,
                "siglip2-giant-opt-patch16-384",
                4,
                FeatureDtype::F16,
                &[1_u8; 8],
            )
            .unwrap();
        assert_eq!(first.offset, HEADER_LEN);
        assert_eq!(second.offset, HEADER_LEN + 8);
        assert_eq!(
            store
                .read(&first.pack_name, first.offset, first.length)
                .unwrap(),
            vec![0_u8; 8]
        );
        assert_eq!(
            store
                .read(&second.pack_name, second.offset, second.length)
                .unwrap(),
            vec![1_u8; 8]
        );
        let map = store.mmap(&first.pack_name).unwrap();
        assert_eq!(map.len(), (HEADER_LEN + 16) as usize);
    }

    #[test]
    fn rejects_mismatched_dimensions_and_ranges() {
        let temp = tempfile::tempdir().unwrap();
        let store = FeatureStore::new();
        store.set_root(temp.path().to_path_buf());
        assert!(
            store
                .append(
                    FeatureKind::Embedding,
                    "model",
                    4,
                    FeatureDtype::F16,
                    &[0_u8; 7]
                )
                .is_err()
        );
        let location = store
            .append(
                FeatureKind::Embedding,
                "model",
                4,
                FeatureDtype::F16,
                &[0_u8; 8],
            )
            .unwrap();
        assert!(
            store
                .read(&location.pack_name, location.offset + 1, location.length)
                .is_err()
        );
    }

    #[test]
    fn truncates_a_half_written_tail_record() {
        let temp = tempfile::tempdir().unwrap();
        let store = FeatureStore::new();
        store.set_root(temp.path().to_path_buf());
        let first = store
            .append(
                FeatureKind::Embedding,
                "model",
                4,
                FeatureDtype::F16,
                &[0_u8; 8],
            )
            .unwrap();
        let path = temp.path().join(&first.pack_name);

        // 模拟掉电：文件末尾多出半个记录。
        store.sync_writers();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[9_u8; 4]).unwrap();
        drop(file);
        store.clear();
        store.set_root(temp.path().to_path_buf());

        let next = store
            .append(
                FeatureKind::Embedding,
                "model",
                4,
                FeatureDtype::F16,
                &[1_u8; 8],
            )
            .unwrap();
        assert_eq!(
            next.offset,
            HEADER_LEN + 8,
            "半条记录要被截掉，新记录从整条边界开始"
        );
        assert_eq!(
            store
                .read(&next.pack_name, next.offset, next.length)
                .unwrap(),
            vec![1_u8; 8]
        );
    }

    #[test]
    fn pack_name_is_stable_and_safe() {
        let name = pack_name(
            FeatureKind::Embedding,
            "SigLIP2/giant opt patch16-384",
            1536,
            FeatureDtype::F16,
        );
        assert!(name.starts_with("embedding-siglip2-giant-opt-patch16-384-1536-f16-"));
        assert!(name.ends_with(".pack"));
        assert!(!name.contains('/'));
    }
}
