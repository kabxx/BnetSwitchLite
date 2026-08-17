use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("未找到 Battle.net 本地账号数据，请先安装并登录一次战网客户端")]
    AccountDatabaseMissing,
    #[error("当前 Battle.net 账号缓存格式不受支持：{0}")]
    AccountDatabaseIncompatible(String),
    #[error("无法读取 Battle.net 账号缓存：{0}")]
    AccountDatabase(String),
    #[error("找不到 Battle.net 客户端，请点击“选择战网客户端”重新指定")]
    ClientExecutableMissing,
    #[error("请选择有效的 Battle.net 客户端")]
    InvalidClientExecutable,
    #[error("应用数据目录不可用：{0}")]
    DataStorage(String),
    #[error("账号快照不可用：{0}")]
    Snapshot(String),
    #[error("切换事务失败：{0}")]
    Transaction(String),
    #[error("登录流程失败：{0}")]
    Login(String),
    #[error("另一个账号操作正在进行，请稍后再试")]
    OperationBusy,
    #[error("战网客户端未能在限定时间内正常退出，已取消操作；不会强制结束进程")]
    ClientStopTimeout,
    #[error("无法启动 Battle.net：{0}")]
    ClientLaunch(String),
    #[error("无法读取系统目录 {path}：{reason}")]
    SystemPath { path: PathBuf, reason: String },
    #[error("操作状态不可用，请重启程序")]
    StateUnavailable,
    #[error("{0}")]
    Message(String),
}

impl AppError {
    pub(crate) fn nested_message(&self) -> String {
        match self {
            Self::Transaction(message) | Self::Login(message) => message.clone(),
            _ => self.to_string(),
        }
    }
}

impl From<AppError> for String {
    fn from(value: AppError) -> Self {
        value.to_string()
    }
}
