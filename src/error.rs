use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),

    #[error("no CMSIS-DAP v2 device found")]
    DeviceNotFound,

    #[error("unexpected DAP response: {0}")]
    InvalidResponse(String),

    #[error("DAP Transfer failed: status={0}, count={1}")]
    TransferFailed(u8, u8),

    #[error("SWD error: {0}")]
    Swd(String),

    #[error("operation timed out")]
    Timeout,

    #[error("failed to start pipeline thread: {0}")]
    PipelineThread(String),

    #[error("pipeline stopped")]
    PipelineStopped,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
