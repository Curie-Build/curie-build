//! Minimal class-file reads used to build OSGi headers.
//!
//! Only the constant pool is walked: `CONSTANT_Class` names become imported
//! packages, and the major version feeds `Require-Capability: osgi.ee`.

use std::collections::BTreeSet;

const CONSTANT_UTF8: u8 = 1;
const CONSTANT_INTEGER: u8 = 3;
const CONSTANT_FLOAT: u8 = 4;
const CONSTANT_LONG: u8 = 5;
const CONSTANT_DOUBLE: u8 = 6;
const CONSTANT_CLASS: u8 = 7;
const CONSTANT_STRING: u8 = 8;
const CONSTANT_FIELDREF: u8 = 9;
const CONSTANT_METHODREF: u8 = 10;
const CONSTANT_INTERFACE_METHODREF: u8 = 11;
const CONSTANT_NAME_AND_TYPE: u8 = 12;
const CONSTANT_METHOD_HANDLE: u8 = 15;
const CONSTANT_METHOD_TYPE: u8 = 16;
const CONSTANT_DYNAMIC: u8 = 17;
const CONSTANT_INVOKE_DYNAMIC: u8 = 18;
const CONSTANT_MODULE: u8 = 19;
const CONSTANT_PACKAGE: u8 = 20;

pub fn class_file_major(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 8 || bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return None;
    }
    Some(u16::from_be_bytes([bytes[6], bytes[7]]))
}

/// Packages referenced by `CONSTANT_Class` entries (dot-separated).
/// Array and primitive descriptors are skipped; `java/lang/String` becomes
/// `java.lang`.
pub fn referenced_packages(bytes: &[u8]) -> BTreeSet<String> {
    let Some(utf8s) = utf8_and_class_indexes(bytes) else {
        return BTreeSet::new();
    };
    let mut pkgs = BTreeSet::new();
    for name in utf8s {
        if let Some(pkg) = package_of_internal(&name) {
            pkgs.insert(pkg);
        }
    }
    pkgs
}

fn package_of_internal(name: &str) -> Option<String> {
    let mut n = name;
    while let Some(rest) = n.strip_prefix('[') {
        n = rest;
    }
    if let Some(obj) = n.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        n = obj;
    }
    if n.len() <= 1 && !n.contains('/') {
        return None;
    }
    let slash = n.rfind('/')?;
    Some(n[..slash].replace('/', "."))
}

fn utf8_and_class_indexes(bytes: &[u8]) -> Option<Vec<String>> {
    if bytes.len() < 10 || bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return None;
    }
    let cp_count = u16::from_be_bytes([bytes[8], bytes[9]]) as usize;
    let mut i = 10usize;
    let mut utf8: Vec<Option<String>> = vec![None; cp_count];
    let mut class_name_idx: Vec<u16> = Vec::new();
    let mut slot = 1usize;
    while slot < cp_count {
        if i >= bytes.len() {
            return None;
        }
        let tag = bytes[i];
        i += 1;
        match tag {
            CONSTANT_UTF8 => {
                if i + 2 > bytes.len() {
                    return None;
                }
                let len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
                i += 2;
                if i + len > bytes.len() {
                    return None;
                }
                utf8[slot] = Some(String::from_utf8_lossy(&bytes[i..i + len]).into_owned());
                i += len;
            }
            CONSTANT_CLASS | CONSTANT_STRING | CONSTANT_METHOD_TYPE | CONSTANT_MODULE
            | CONSTANT_PACKAGE => {
                if i + 2 > bytes.len() {
                    return None;
                }
                let idx = u16::from_be_bytes([bytes[i], bytes[i + 1]]);
                if tag == CONSTANT_CLASS {
                    class_name_idx.push(idx);
                }
                i += 2;
            }
            CONSTANT_METHOD_HANDLE => {
                i = i.checked_add(3)?;
            }
            CONSTANT_INTEGER
            | CONSTANT_FLOAT
            | CONSTANT_FIELDREF
            | CONSTANT_METHODREF
            | CONSTANT_INTERFACE_METHODREF
            | CONSTANT_NAME_AND_TYPE
            | CONSTANT_DYNAMIC
            | CONSTANT_INVOKE_DYNAMIC => {
                i = i.checked_add(4)?;
            }
            CONSTANT_LONG | CONSTANT_DOUBLE => {
                i = i.checked_add(8)?;
                slot += 1;
            }
            _ => return None,
        }
        slot += 1;
    }
    let mut names = Vec::new();
    for idx in class_name_idx {
        if let Some(Some(s)) = utf8.get(idx as usize) {
            names.push(s.clone());
        }
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_of_internal_class_and_array() {
        assert_eq!(
            package_of_internal("com/example/Foo"),
            Some("com.example".into())
        );
        assert_eq!(
            package_of_internal("[Ljava/lang/String;"),
            Some("java.lang".into())
        );
        assert_eq!(package_of_internal("I"), None);
        assert_eq!(package_of_internal("Foo"), None);
    }

    #[test]
    fn referenced_packages_from_minimal_class() {
        // Hand-built CP: #1 Utf8 java/lang/String, #2 Class #1
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 65];
        b.extend_from_slice(&3u16.to_be_bytes()); // cp_count = 3
                                                  // #1 Utf8
        b.push(CONSTANT_UTF8);
        let name = b"java/lang/String";
        b.extend_from_slice(&(name.len() as u16).to_be_bytes());
        b.extend_from_slice(name);
        // #2 Class -> #1
        b.push(CONSTANT_CLASS);
        b.extend_from_slice(&1u16.to_be_bytes());
        let pkgs = referenced_packages(&b);
        assert_eq!(pkgs, BTreeSet::from(["java.lang".to_string()]));
    }
}
