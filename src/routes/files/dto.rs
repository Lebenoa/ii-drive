use crate::db::FileRow;

#[derive(serde::Serialize)]
pub struct FileDto {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub created_at: i64,
    pub public: bool,
    /// true when a Telegram thumbnail exists for this file.
    pub has_thumb: bool,
}

impl FileDto {
    /// `has_thumb` comes from the thumbnail store on disk, not the row:
    /// the database no longer carries preview bytes.
    pub fn new(r: FileRow, has_thumb: bool) -> Self {
        Self {
            id: r.uid,
            name: r.name,
            mime: r.mime,
            size: r.size,
            created_at: r.created_at,
            public: r.public,
            has_thumb,
        }
    }
}

#[derive(serde::Deserialize)]
pub struct VisibilityBody {
    pub public: bool,
}

#[derive(serde::Deserialize)]
pub struct MoveBody {
    /// Target folder id, "" = root.
    pub folder: String,
}

#[derive(serde::Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    /// Folder id to list; missing/empty means root.
    #[serde(default)]
    pub folder: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct CreateFolderBody {
    pub name: String,
    #[serde(default)]
    pub parent: String,
}
