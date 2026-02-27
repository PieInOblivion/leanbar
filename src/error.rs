use thiserror::Error;

#[derive(Error, Debug)]
pub enum LeanbarError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Font error: {0}")]
    Font(String),

    #[error("Atlas error: {0}")]
    Atlas(String),

    #[error("XDG_CACHE_HOME or HOME not set")]
    NoHome,

    #[error("Integer parse error: {0}")]
    ParseInt(#[from] std::num::ParseIntError),

    #[error("Float parse error: {0}")]
    ParseFloat(#[from] std::num::ParseFloatError),

    #[error("Wayland connection error: {0}")]
    WaylandConnect(#[from] wayland_client::ConnectError),

    #[error("Wayland dispatch error: {0}")]
    WaylandDispatch(#[from] wayland_client::DispatchError),

    #[error("Wayland error: {0}")]
    Wayland(String),

    #[error("Rustix error: {0}")]
    Rustix(#[from] rustix::io::Errno),

    #[error("Buffer conversion error: {0}")]
    SliceConversion(#[from] std::array::TryFromSliceError),

    #[error("UTF-8 conversion error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}
