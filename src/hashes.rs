//! Cryptographic hashes are provided by Windows CNG; no bundled crypto runtime.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct Hashes {
    pub sha1: String,
    pub sha256: String,
    pub sha512: String,
    pub md5: String,
}

impl Hashes {
    pub fn of(data: &[u8]) -> Result<Self> {
        Ok(Self {
            sha1: digest("SHA1", data, 20)?,
            sha256: digest("SHA256", data, 32)?,
            sha512: digest("SHA512", data, 64)?,
            md5: digest("MD5", data, 16)?,
        })
    }
    pub fn agrees(&self, hashes: &serde_json::Value) -> bool {
        let mut compared = false;
        for (name, expected) in [
            ("sha1", &self.sha1),
            ("sha512", &self.sha512),
            ("md5", &self.md5),
        ] {
            if let Some(actual) = hashes.get(name).and_then(|x| x.as_str()) {
                compared = true;
                if !expected.eq_ignore_ascii_case(actual) {
                    return false;
                }
            }
        }
        compared
    }
}

#[cfg(not(windows))]
fn digest(_: &str, _: &[u8], _: usize) -> Result<String> {
    bail!("CNG 哈希需要 Windows")
}

#[cfg(windows)]
fn digest(name: &str, data: &[u8], size: usize) -> Result<String> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Security::Cryptography::*;
    struct Algorithm(BCRYPT_ALG_HANDLE);
    impl Drop for Algorithm {
        fn drop(&mut self) {
            unsafe {
                BCryptCloseAlgorithmProvider(self.0, 0);
            }
        }
    }
    struct Hash(BCRYPT_HASH_HANDLE);
    impl Drop for Hash {
        fn drop(&mut self) {
            unsafe {
                BCryptDestroyHash(self.0);
            }
        }
    }
    unsafe {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut algorithm = null_mut();
        if BCryptOpenAlgorithmProvider(&mut algorithm, name.as_ptr(), null(), 0) < 0 {
            bail!("无法初始化哈希算法");
        }
        let algorithm = Algorithm(algorithm);
        let mut hash = null_mut();
        if BCryptCreateHash(algorithm.0, &mut hash, null_mut(), 0, null(), 0, 0) < 0 {
            bail!("无法创建哈希");
        }
        let hash = Hash(hash);
        if BCryptHashData(hash.0, data.as_ptr(), data.len() as u32, 0) < 0 {
            bail!("哈希计算失败");
        }
        let mut output = vec![0u8; size];
        if BCryptFinishHash(hash.0, output.as_mut_ptr(), size as u32, 0) < 0 {
            bail!("哈希计算失败");
        }
        Ok(output.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

pub fn fingerprint(bytes: &[u8]) -> u32 {
    let data: Vec<u8> = bytes
        .iter()
        .copied()
        .filter(|x| !matches!(x, 9 | 10 | 13 | 32))
        .collect();
    let mut value = 1u32 ^ data.len() as u32;
    let mut chunks = data.chunks_exact(4);
    for chunk in &mut chunks {
        let mut key = u32::from_le_bytes(chunk.try_into().unwrap()).wrapping_mul(0x5bd1e995);
        key ^= key >> 24;
        value = value.wrapping_mul(0x5bd1e995) ^ key.wrapping_mul(0x5bd1e995);
    }
    let tail = chunks.remainder();
    if !tail.is_empty() {
        for (i, byte) in tail.iter().enumerate() {
            value ^= (*byte as u32) << (i * 8);
        }
        value = value.wrapping_mul(0x5bd1e995);
    }
    value ^= value >> 13;
    value = value.wrapping_mul(0x5bd1e995);
    value ^ (value >> 15)
}
