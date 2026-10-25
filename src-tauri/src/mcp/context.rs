use std::{path::PathBuf, sync::Arc};

use crate::{
    app::settings::Settings,
    library::{db::LibraryDb, service::LibraryInfo, thumbs::ThumbPack},
};

use super::sheet::SheetCache;

/// MCP 只读工具共用的一份图库快照；不经过 Tauri IPC，也不复制领域逻辑。
pub struct ReadContext {
    pub database: Arc<LibraryDb>,
    pub thumbs: Arc<ThumbPack>,
    pub root: PathBuf,
    pub info: LibraryInfo,
    pub settings: Settings,
    pub sheets: Arc<parking_lot::Mutex<SheetCache>>,
}
