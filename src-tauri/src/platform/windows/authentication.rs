use std::{collections::BTreeMap, ptr, thread, time::Duration};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::AppError;

const UNIFIED_AUTH_PATH: &str = "Software\\Blizzard Entertainment\\Battle.net\\UnifiedAuth";
const HKEY_CURRENT_USER: isize = 0x8000_0001_u32 as i32 as isize;
const KEY_QUERY_VALUE: u32 = 0x0001;
const ERROR_SUCCESS: i32 = 0;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_MORE_DATA: i32 = 234;
const ERROR_NO_MORE_ITEMS: i32 = 259;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthenticationBaseline {
    values: BTreeMap<String, String>,
}

impl AuthenticationBaseline {
    pub(crate) fn capture() -> Result<Self, AppError> {
        for _ in 0..3 {
            let first = read_registry_values()?;
            thread::sleep(Duration::from_millis(20));
            let second = read_registry_values()?;
            if first == second {
                return Ok(Self { values: second });
            }
        }
        Err(AppError::Login(
            "战网认证状态正在变化，暂时无法稳定读取".into(),
        ))
    }

    pub(crate) fn has_fresh_value_since(&self, current: &Self) -> bool {
        current
            .values
            .iter()
            .any(|(name, fingerprint)| self.values.get(name) != Some(fingerprint))
    }

    #[cfg(test)]
    fn from_entries(entries: &[(&str, &str)]) -> Self {
        Self {
            values: entries
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }
}

struct RegistryKey(isize);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn read_registry_values() -> Result<BTreeMap<String, String>, AppError> {
    let path = UNIFIED_AUTH_PATH
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut raw_key = 0_isize;
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut raw_key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(BTreeMap::new());
    }
    if status != ERROR_SUCCESS {
        return Err(registry_error("无法打开战网认证注册表", status));
    }
    let key = RegistryKey(raw_key);

    for _ in 0..3 {
        match enumerate_values(&key) {
            Ok(values) => return Ok(values),
            Err(RegistryReadError::Changed) => continue,
            Err(RegistryReadError::Status(status)) => {
                return Err(registry_error("无法读取战网认证注册表", status));
            }
        }
    }
    Err(AppError::Login(
        "战网认证状态正在变化，暂时无法建立登录基线".into(),
    ))
}

enum RegistryReadError {
    Changed,
    Status(i32),
}

fn enumerate_values(key: &RegistryKey) -> Result<BTreeMap<String, String>, RegistryReadError> {
    let mut value_count = 0_u32;
    let mut max_name_length = 0_u32;
    let mut max_data_length = 0_u32;
    let status = unsafe {
        RegQueryInfoKeyW(
            key.0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut value_count,
            &mut max_name_length,
            &mut max_data_length,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(RegistryReadError::Status(status));
    }

    let mut values = BTreeMap::new();
    for index in 0..value_count.saturating_add(1) {
        let mut name = vec![0_u16; max_name_length.saturating_add(2) as usize];
        let mut name_length = name.len() as u32;
        let mut data = vec![0_u8; max_data_length.max(1) as usize];
        let mut data_length = data.len() as u32;
        let mut value_type = 0_u32;
        let status = unsafe {
            RegEnumValueW(
                key.0,
                index,
                name.as_mut_ptr(),
                &mut name_length,
                ptr::null_mut(),
                &mut value_type,
                data.as_mut_ptr(),
                &mut data_length,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        if status == ERROR_MORE_DATA {
            return Err(RegistryReadError::Changed);
        }
        if status != ERROR_SUCCESS {
            return Err(RegistryReadError::Status(status));
        }
        name.truncate(name_length as usize);
        data.truncate(data_length as usize);
        let name = String::from_utf16(&name)
            .map_err(|_| RegistryReadError::Status(ERROR_MORE_DATA))?
            .to_ascii_uppercase();
        let mut hasher = Sha256::new();
        hasher.update(value_type.to_le_bytes());
        hasher.update(data);
        values.insert(name, format!("{:x}", hasher.finalize()));
    }
    Ok(values)
}

fn registry_error(context: &str, status: i32) -> AppError {
    AppError::Login(format!("{context}（Windows 错误 {status}）"))
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        hkey: isize,
        sub_key: *const u16,
        options: u32,
        desired_access: u32,
        result: *mut isize,
    ) -> i32;
    fn RegQueryInfoKeyW(
        hkey: isize,
        class: *mut u16,
        class_length: *mut u32,
        reserved: *mut u32,
        sub_key_count: *mut u32,
        max_sub_key_length: *mut u32,
        max_class_length: *mut u32,
        value_count: *mut u32,
        max_value_name_length: *mut u32,
        max_value_data_length: *mut u32,
        security_descriptor_length: *mut u32,
        last_write_time: *mut u8,
    ) -> i32;
    fn RegEnumValueW(
        hkey: isize,
        index: u32,
        value_name: *mut u16,
        value_name_length: *mut u32,
        reserved: *mut u32,
        value_type: *mut u32,
        data: *mut u8,
        data_length: *mut u32,
    ) -> i32;
    fn RegCloseKey(hkey: isize) -> i32;
}

#[cfg(test)]
mod tests {
    use super::AuthenticationBaseline;

    #[test]
    fn deletion_alone_is_not_fresh_authentication() {
        let before = AuthenticationBaseline::from_entries(&[("A", "one"), ("B", "two")]);
        let after = AuthenticationBaseline::from_entries(&[("A", "one")]);
        assert!(!before.has_fresh_value_since(&after));
    }

    #[test]
    fn new_or_replaced_value_is_fresh_authentication() {
        let before = AuthenticationBaseline::from_entries(&[("A", "one")]);
        let replaced = AuthenticationBaseline::from_entries(&[("A", "updated")]);
        let added = AuthenticationBaseline::from_entries(&[("A", "one"), ("B", "new")]);
        assert!(before.has_fresh_value_since(&replaced));
        assert!(before.has_fresh_value_since(&added));
    }
}
