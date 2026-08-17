use std::ptr;

use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    data::CFData,
    dictionary::{CFDictionary, CFMutableDictionary},
    propertylist::{
        self, CFPropertyList, kCFPropertyListBinaryFormat_v1_0, kCFPropertyListImmutable,
    },
    string::CFString,
};
use core_foundation_sys::preferences::{
    CFPreferencesCopyKeyList, CFPreferencesCopyMultiple, CFPreferencesSetMultiple,
    CFPreferencesSynchronize, kCFPreferencesAnyHost, kCFPreferencesCurrentHost,
    kCFPreferencesCurrentUser,
};
use serde::{Deserialize, Serialize};

use crate::error::AppError;

const DOMAIN: &str = "net.battle";
const AUTH_PREFIX: &str = "UnifiedAuth/";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreferenceSnapshot {
    pub any_host: Vec<u8>,
    pub current_host: Vec<u8>,
}

#[derive(Clone, Copy)]
enum HostScope {
    Any,
    Current,
}

impl PreferenceSnapshot {
    pub fn has_authentication_keys(&self) -> Result<bool, AppError> {
        Ok(dictionary_keys(&parse_dictionary(&self.any_host)?)?
            .into_iter()
            .chain(dictionary_keys(&parse_dictionary(&self.current_host)?)?)
            .any(|key| key.starts_with(AUTH_PREFIX)))
    }
}

pub(crate) fn capture() -> Result<PreferenceSnapshot, AppError> {
    Ok(PreferenceSnapshot {
        any_host: serialize_dictionary(&copy_scope(HostScope::Any)?)?,
        current_host: serialize_dictionary(&copy_scope(HostScope::Current)?)?,
    })
}

pub(crate) fn clear_all() -> Result<(), AppError> {
    replace_scope(HostScope::Any, &empty_dictionary())?;
    replace_scope(HostScope::Current, &empty_dictionary())?;
    Ok(())
}

pub(crate) fn replace(snapshot: &PreferenceSnapshot) -> Result<(), AppError> {
    let any_host = parse_dictionary(&snapshot.any_host)?;
    let current_host = parse_dictionary(&snapshot.current_host)?;
    clear_all()?;
    replace_scope(HostScope::Any, &any_host)?;
    replace_scope(HostScope::Current, &current_host)?;
    let actual = capture()?;
    if !dictionaries_equal(&snapshot.any_host, &actual.any_host)?
        || !dictionaries_equal(&snapshot.current_host, &actual.current_host)?
    {
        return Err(AppError::Transaction(
            "macOS 战网偏好域写入后校验不一致".into(),
        ));
    }
    Ok(())
}

pub(crate) fn clear_authentication_only() -> Result<(), AppError> {
    for scope in [HostScope::Any, HostScope::Current] {
        let current = copy_scope(scope)?;
        let keys = dictionary_keys(&current)?;
        let auth_keys = keys
            .into_iter()
            .filter(|key| key.starts_with(AUTH_PREFIX))
            .map(|key| CFString::new(&key))
            .collect::<Vec<_>>();
        if !auth_keys.is_empty() {
            let remove = CFArray::from_CFTypes(&auth_keys);
            synchronize_set(scope, ptr::null(), remove.as_concrete_TypeRef())?;
        }
        let after = copy_scope(scope)?;
        if dictionary_keys(&after)?
            .iter()
            .any(|key| key.starts_with(AUTH_PREFIX))
            || !non_authentication_equal(
                &serialize_dictionary(&current)?,
                &serialize_dictionary(&after)?,
            )?
        {
            return Err(AppError::Login("无法确认 macOS 战网认证偏好已清除".into()));
        }
    }
    Ok(())
}

fn replace_scope(
    scope: HostScope,
    target: &CFDictionary<CFString, CFType>,
) -> Result<(), AppError> {
    let current_keys = key_list(scope)?;
    if !current_keys.is_empty() {
        synchronize_set(scope, ptr::null(), current_keys.as_concrete_TypeRef())?;
    }
    synchronize_set(scope, target.as_concrete_TypeRef(), ptr::null())?;
    let actual = copy_scope(scope)?;
    if actual.as_CFType() != target.as_CFType() {
        return Err(AppError::Transaction(
            "macOS 战网偏好域替换后校验失败".into(),
        ));
    }
    Ok(())
}

fn synchronize_set(
    scope: HostScope,
    values: core_foundation_sys::dictionary::CFDictionaryRef,
    removals: core_foundation_sys::array::CFArrayRef,
) -> Result<(), AppError> {
    let application = CFString::new(DOMAIN);
    let user = unsafe { CFString::wrap_under_get_rule(kCFPreferencesCurrentUser) };
    let host = host(scope);
    unsafe {
        CFPreferencesSetMultiple(
            values,
            removals,
            application.as_concrete_TypeRef(),
            user.as_concrete_TypeRef(),
            host.as_concrete_TypeRef(),
        );
    }
    if unsafe {
        CFPreferencesSynchronize(
            application.as_concrete_TypeRef(),
            user.as_concrete_TypeRef(),
            host.as_concrete_TypeRef(),
        )
    } == 0
    {
        return Err(AppError::Transaction("macOS 拒绝同步战网偏好域".into()));
    }
    Ok(())
}

fn copy_scope(scope: HostScope) -> Result<CFDictionary<CFString, CFType>, AppError> {
    let application = CFString::new(DOMAIN);
    let user = unsafe { CFString::wrap_under_get_rule(kCFPreferencesCurrentUser) };
    let host = host(scope);
    let keys = key_list(scope)?;
    if keys.is_empty() {
        return Ok(empty_dictionary());
    }
    let dictionary = unsafe {
        CFPreferencesCopyMultiple(
            keys.as_concrete_TypeRef(),
            application.as_concrete_TypeRef(),
            user.as_concrete_TypeRef(),
            host.as_concrete_TypeRef(),
        )
    };
    if dictionary.is_null() {
        return Err(AppError::Snapshot("无法读取 macOS 战网偏好域".into()));
    }
    Ok(unsafe { CFDictionary::wrap_under_create_rule(dictionary) })
}

fn key_list(scope: HostScope) -> Result<CFArray<CFString>, AppError> {
    let application = CFString::new(DOMAIN);
    let user = unsafe { CFString::wrap_under_get_rule(kCFPreferencesCurrentUser) };
    let host = host(scope);
    let keys = unsafe {
        CFPreferencesCopyKeyList(
            application.as_concrete_TypeRef(),
            user.as_concrete_TypeRef(),
            host.as_concrete_TypeRef(),
        )
    };
    if keys.is_null() {
        Ok(CFArray::from_CFTypes(&[]))
    } else {
        Ok(unsafe { CFArray::wrap_under_create_rule(keys) })
    }
}

fn host(scope: HostScope) -> CFString {
    unsafe {
        CFString::wrap_under_get_rule(match scope {
            HostScope::Any => kCFPreferencesAnyHost,
            HostScope::Current => kCFPreferencesCurrentHost,
        })
    }
}

fn empty_dictionary() -> CFDictionary<CFString, CFType> {
    CFDictionary::from_CFType_pairs(&[])
}

fn serialize_dictionary(dictionary: &CFDictionary<CFString, CFType>) -> Result<Vec<u8>, AppError> {
    propertylist::create_data(dictionary.as_CFTypeRef(), kCFPropertyListBinaryFormat_v1_0)
        .map(|data| data.bytes().to_vec())
        .map_err(|_| AppError::Snapshot("无法序列化 macOS 战网偏好域".into()))
}

fn parse_dictionary(bytes: &[u8]) -> Result<CFDictionary<CFString, CFType>, AppError> {
    let (property_list, _) =
        propertylist::create_with_data(CFData::from_buffer(bytes), kCFPropertyListImmutable)
            .map_err(|_| AppError::Snapshot("认证快照中的偏好域格式无效".into()))?;
    let property_list = unsafe { CFPropertyList::wrap_under_create_rule(property_list) };
    let Some(dictionary) = property_list.into_CFType().downcast_into::<CFDictionary>() else {
        return Err(AppError::Snapshot("认证快照中的偏好域不是字典".into()));
    };
    Ok(unsafe { CFDictionary::wrap_under_get_rule(dictionary.as_CFTypeRef().cast()) })
}

fn dictionary_keys(dictionary: &CFDictionary<CFString, CFType>) -> Result<Vec<String>, AppError> {
    let (keys, _) = dictionary.get_keys_and_values();
    keys.into_iter()
        .map(|key| {
            if key.is_null() {
                return Err(AppError::Snapshot("战网偏好域包含空键".into()));
            }
            let key =
                unsafe { CFType::wrap_under_get_rule(key.cast_mut()).downcast_into::<CFString>() }
                    .ok_or_else(|| AppError::Snapshot("战网偏好域包含非文本键".into()))?;
            Ok(key.to_string())
        })
        .collect()
}

fn dictionaries_equal(first: &[u8], second: &[u8]) -> Result<bool, AppError> {
    Ok(parse_dictionary(first)?.as_CFType() == parse_dictionary(second)?.as_CFType())
}

fn non_authentication_equal(first: &[u8], second: &[u8]) -> Result<bool, AppError> {
    let first = parse_dictionary(first)?;
    let second = parse_dictionary(second)?;
    let mut first = CFMutableDictionary::from(&first);
    let mut second = CFMutableDictionary::from(&second);
    for key in dictionary_keys(&first.to_immutable())? {
        if key.starts_with(AUTH_PREFIX) {
            first.remove(CFString::new(&key));
        }
    }
    for key in dictionary_keys(&second.to_immutable())? {
        if key.starts_with(AUTH_PREFIX) {
            second.remove(CFString::new(&key));
        }
    }
    Ok(first.to_immutable().as_CFType() == second.to_immutable().as_CFType())
}

#[cfg(test)]
mod tests {
    use super::non_authentication_equal;

    fn dictionary(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut dictionary = plist::Dictionary::new();
        for (key, value) in entries {
            dictionary.insert((*key).into(), plist::Value::String((*value).into()));
        }
        let mut bytes = Vec::new();
        plist::to_writer_binary(&mut bytes, &plist::Value::Dictionary(dictionary)).unwrap();
        bytes
    }

    #[test]
    fn authentication_entries_are_ignored_but_other_preferences_must_match() {
        let before = dictionary(&[("UnifiedAuth/one", "secret"), ("Locale", "zhCN")]);
        let after = dictionary(&[("Locale", "zhCN")]);
        assert!(non_authentication_equal(&before, &after).unwrap());

        let changed = dictionary(&[("Locale", "enUS")]);
        assert!(!non_authentication_equal(&before, &changed).unwrap());
    }
}
