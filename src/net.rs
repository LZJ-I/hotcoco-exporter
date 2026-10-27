//! Bounded WinHTTP requests using Windows TLS and optional explicit proxy settings.
#[cfg(windows)]
use anyhow::anyhow;
use anyhow::{bail, Context, Result};
use serde_json::Value;

#[derive(Clone, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Client {
    pub proxy: Option<String>,
}

pub fn allowed_download(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url)?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some_and(|p| p != 443)
        || parsed.fragment().is_some()
        || !matches!(
            parsed.host_str(),
            Some(
                "cdn.modrinth.com"
                    | "edge.forgecdn.net"
                    | "mediafilez.forgecdn.net"
                    | "media.forgecdn.net"
            )
        )
    {
        bail!("不支持的下载地址：{url}");
    }
    Ok(())
}

impl Client {
    pub fn json(&self, url: &str, body: Option<&Value>) -> Result<(u32, Value)> {
        let payload = body.map(serde_json::to_vec).transpose()?;
        let (status, data) = self.request(url, payload.as_deref(), 32 * 1024 * 1024, false)?;
        if status == 404 {
            return Ok((status, Value::Null));
        }
        if status != 200 {
            bail!("接口返回 HTTP {status}");
        }
        Ok((
            status,
            serde_json::from_slice(&data).context("接口返回了无效 JSON")?,
        ))
    }

    pub fn download(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        allowed_download(url)?;
        let (status, bytes) = self.request(url, None, max, true)?;
        if status != 200 {
            bail!("下载返回 HTTP {status}");
        }
        Ok(bytes)
    }

    #[cfg(not(windows))]
    fn request(&self, _: &str, _: Option<&[u8]>, _: usize, _: bool) -> Result<(u32, Vec<u8>)> {
        bail!("网络功能需要 Windows；Linux 构建用于核心测试")
    }

    #[cfg(windows)]
    fn request(
        &self,
        address: &str,
        body: Option<&[u8]>,
        max: usize,
        resource: bool,
    ) -> Result<(u32, Vec<u8>)> {
        use std::ptr::{null, null_mut};
        use windows_sys::Win32::{Foundation::GetLastError, Networking::WinHttp::*};
        struct Handle(*mut core::ffi::c_void);
        impl Drop for Handle {
            fn drop(&mut self) {
                unsafe {
                    WinHttpCloseHandle(self.0);
                }
            }
        }
        fn checked(value: *mut core::ffi::c_void) -> Result<Handle> {
            if value.is_null() {
                Err(anyhow!("WinHTTP 错误 {}", unsafe { GetLastError() }))
            } else {
                Ok(Handle(value))
            }
        }
        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(Some(0)).collect()
        }
        let mut address = address.to_string();
        for _ in 0..=4 {
            let url = url::Url::parse(&address)?;
            if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
                bail!("仅支持 HTTPS");
            }
            if resource {
                allowed_download(&address)?;
            }
            let proxy = self.proxy.as_deref().unwrap_or("").trim();
            let proxy = proxy
                .strip_prefix("http://")
                .unwrap_or(proxy)
                .trim_end_matches('/');
            let proxy_wide = wide(proxy);
            unsafe {
                let session = checked(WinHttpOpen(
                    wide("HotCoco-Exporter/0.2 (+https://github.com)").as_ptr(),
                    if proxy.is_empty() {
                        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
                    } else {
                        WINHTTP_ACCESS_TYPE_NAMED_PROXY
                    },
                    if proxy.is_empty() {
                        null()
                    } else {
                        proxy_wide.as_ptr()
                    },
                    null(),
                    0,
                ))?;
                WinHttpSetTimeouts(session.0, 10000, 10000, 20000, 20000);
                let connect = checked(WinHttpConnect(
                    session.0,
                    wide(url.host_str().context("地址缺少主机")?).as_ptr(),
                    url.port_or_known_default().unwrap_or(443),
                    0,
                ))?;
                let mut path = url.path().to_string();
                if let Some(query) = url.query() {
                    path.push('?');
                    path.push_str(query);
                }
                let request = checked(WinHttpOpenRequest(
                    connect.0,
                    wide(if body.is_some() { "POST" } else { "GET" }).as_ptr(),
                    wide(&path).as_ptr(),
                    null(),
                    null(),
                    null(),
                    WINHTTP_FLAG_SECURE,
                ))?;
                let redirect: u32 = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
                if WinHttpSetOption(
                    request.0,
                    WINHTTP_OPTION_REDIRECT_POLICY,
                    &redirect as *const _ as *mut _,
                    4,
                ) == 0
                {
                    bail!("无法设置重定向策略");
                }
                let headers =
                    wide("Accept: application/json\r\nContent-Type: application/json\r\n");
                let payload = body.unwrap_or_default();
                if WinHttpSendRequest(
                    request.0,
                    headers.as_ptr(),
                    (headers.len() - 1) as u32,
                    if payload.is_empty() {
                        null()
                    } else {
                        payload.as_ptr().cast()
                    },
                    payload.len() as u32,
                    payload.len() as u32,
                    0,
                ) == 0
                    || WinHttpReceiveResponse(request.0, null_mut()) == 0
                {
                    bail!("网络请求失败（{}）", GetLastError());
                }
                let mut status: u32 = 0;
                let mut length = 4;
                if WinHttpQueryHeaders(
                    request.0,
                    WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                    null(),
                    &mut status as *mut _ as *mut _,
                    &mut length,
                    null_mut(),
                ) == 0
                {
                    bail!("无法读取 HTTP 状态");
                }
                if matches!(status, 301 | 302 | 303 | 307 | 308) {
                    if body.is_some() {
                        bail!("识别接口意外重定向");
                    }
                    let mut location = vec![0u16; 8192];
                    let mut size = (location.len() * 2) as u32;
                    if WinHttpQueryHeaders(
                        request.0,
                        WINHTTP_QUERY_LOCATION,
                        null(),
                        location.as_mut_ptr().cast(),
                        &mut size,
                        null_mut(),
                    ) == 0
                    {
                        bail!("无效下载重定向");
                    }
                    let location = String::from_utf16_lossy(
                        &location[..location
                            .iter()
                            .position(|x| *x == 0)
                            .unwrap_or(location.len())],
                    );
                    address = url.join(&location)?.to_string();
                    continue;
                }
                let mut result = Vec::new();
                let mut buffer = [0u8; 65536];
                loop {
                    let mut read = 0;
                    if WinHttpReadData(
                        request.0,
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        &mut read,
                    ) == 0
                    {
                        bail!("下载中断（{}）", GetLastError());
                    }
                    if read == 0 {
                        break;
                    }
                    if result.len() + read as usize > max {
                        bail!("下载超过允许大小");
                    }
                    result.extend_from_slice(&buffer[..read as usize]);
                }
                return Ok((status, result));
            }
        }
        bail!("下载重定向过多")
    }
}
