use std::path::Path;

use objc2_foundation::{NSNumber, NSString, NSURL, NSURLIsExcludedFromBackupKey};

pub(crate) fn set_and_verify(path: &Path) -> Result<(), String> {
    let path = NSString::from_str(&path.to_string_lossy());
    let url = NSURL::fileURLWithPath(&path);
    let enabled = NSNumber::numberWithBool(true);
    unsafe {
        url.setResourceValue_forKey_error(Some(enabled.as_ref()), NSURLIsExcludedFromBackupKey)
    }
    .map_err(|_| "无法将敏感数据目录排除在系统备份之外".to_owned())?;

    let mut actual = None;
    unsafe { url.getResourceValue_forKey_error(&mut actual, NSURLIsExcludedFromBackupKey) }
        .map_err(|_| "无法验证敏感数据目录的备份排除属性".to_owned())?;
    let excluded = actual
        .as_deref()
        .and_then(|value| value.downcast_ref::<NSNumber>())
        .is_some_and(NSNumber::boolValue);
    if !excluded {
        return Err("敏感数据目录未被系统备份排除，已停止创建认证快照".to_owned());
    }
    Ok(())
}
