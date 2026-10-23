use std::fmt;

#[derive(Debug)]
pub enum AppError {
    Message(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Database(rusqlite::Error),
    Image(image::ImageError),
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn message(value: impl Into<String>) -> Self {
        Self::Message(value.into())
    }

    pub fn command<T>(result: AppResult<T>) -> Result<T, String> {
        result.map_err(|error| error.to_string())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(value) => formatter.write_str(value),
            Self::Io(error) => write!(formatter, "文件系统错误：{error}"),
            Self::Json(error) => write!(formatter, "JSON 错误：{error}"),
            Self::Database(error) => write!(formatter, "数据库错误：{error}"),
            Self::Image(error) => write!(formatter, "图像错误：{error}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for AppError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl From<image::ImageError> for AppError {
    fn from(error: image::ImageError) -> Self {
        Self::Image(error)
    }
}
