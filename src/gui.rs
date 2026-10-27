//! Native Win32 UI. Worker threads communicate through a queue; no UI thread networking.
use crate::{
    core::{self, Cancel, Row},
    instances::{self, Discovery, Instance},
    net::Client,
};
use anyhow::{bail, Result};
use std::{
    mem::{size_of, zeroed},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, Sender},
    time::{SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::*,
        HiDpi::*,
        Input::KeyboardAndMouse::EnableWindow,
        Shell::{DragAcceptFiles, DragFinish, DragQueryFileW},
        WindowsAndMessaging::*,
    },
};

const CHOOSE: i32 = 101;
const SCAN: i32 = 102;
const EXPORT: i32 = 103;
const OWN: i32 = 104;
const VERIFY: i32 = 105;
const AUTO: i32 = 106;
const PACK: i32 = 206;
const BG: u32 = 0xfaf8f7;
const INK: u32 = 0x433025;
const MUTED: u32 = 0x90837d;
const ACCENT: u32 = 0xd96375;
const LINE: u32 = 0xf0e8e5;
const WHITE: u32 = 0xffffff;

enum Event {
    Row(Row, usize, usize),
    Progress(String, usize),
    End(Result<String>),
}

struct State {
    hwnd: HWND,
    path: HWND,
    proxy: HWND,
    version: HWND,
    list: HWND,
    progress: HWND,
    packs: HWND,
    instances: Vec<Instance>,
    resolving: bool,
    directory_dirty: bool,
    buttons: Vec<(i32, HWND)>,
    rows: Vec<Row>,
    root: PathBuf,
    busy: bool,
    complete: bool,
    status: String,
    percent: usize,
    width: i32,
    height: i32,
    scale: f32,
    font: HFONT,
    small: HFONT,
    bold: HFONT,
    title: HFONT,
    sender: Sender<Event>,
    receiver: Receiver<Event>,
    cancel: Cancel,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn scale(s: &State, n: i32) -> i32 {
    (n as f32 * s.scale).round() as i32
}
unsafe fn fitting_scale(requested: f32) -> f32 {
    let mut work: RECT = zeroed();
    if SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0) != 0 {
        requested
            .min((work.right - work.left - 40) as f32 / 960.0)
            .min((work.bottom - work.top - 60) as f32 / 784.0)
            .max(0.75)
    } else {
        requested
    }
}
fn rect(s: &State, x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: scale(s, x),
        top: scale(s, y),
        right: scale(s, x + w),
        bottom: scale(s, y + h),
    }
}
unsafe fn text(
    hdc: HDC,
    s: &State,
    value: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    font: HFONT,
    color: u32,
) {
    let previous = SelectObject(hdc, font);
    SetBkMode(hdc, TRANSPARENT as i32);
    SetTextColor(hdc, color);
    let mut area = rect(s, x, y, w, h);
    let mut value = wide(value);
    DrawTextW(
        hdc,
        value.as_mut_ptr(),
        -1,
        &mut area,
        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    SelectObject(hdc, previous);
}
unsafe fn card(
    hdc: HDC,
    s: &State,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
    border: u32,
    radius: i32,
) {
    let brush = CreateSolidBrush(color);
    let pen = CreatePen(PS_SOLID, 1, border);
    let old_brush = SelectObject(hdc, brush);
    let old_pen = SelectObject(hdc, pen);
    let r = rect(s, x, y, w, h);
    RoundRect(
        hdc,
        r.left,
        r.top,
        r.right,
        r.bottom,
        scale(s, radius),
        scale(s, radius),
    );
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    DeleteObject(brush);
    DeleteObject(pen);
}
unsafe fn button(
    hdc: HDC,
    s: &State,
    area: RECT,
    label: &str,
    primary: bool,
    disabled: bool,
    pressed: bool,
) {
    let brush = CreateSolidBrush(if disabled {
        0xf0eeed
    } else if primary {
        if pressed {
            0xb84b5f
        } else {
            ACCENT
        }
    } else {
        0xf9f5f3
    });
    let pen = CreatePen(
        PS_SOLID,
        1,
        if primary && !disabled { ACCENT } else { LINE },
    );
    let a = SelectObject(hdc, brush);
    let b = SelectObject(hdc, pen);
    RoundRect(
        hdc,
        area.left,
        area.top,
        area.right,
        area.bottom,
        scale(s, 12),
        scale(s, 12),
    );
    SelectObject(hdc, a);
    SelectObject(hdc, b);
    DeleteObject(brush);
    DeleteObject(pen);
    let old = SelectObject(hdc, s.bold);
    SetBkMode(hdc, TRANSPARENT as i32);
    SetTextColor(
        hdc,
        if disabled {
            0xb9adaa
        } else if primary {
            WHITE
        } else {
            INK
        },
    );
    let mut value = wide(label);
    let mut r = area;
    DrawTextW(
        hdc,
        value.as_mut_ptr(),
        -1,
        &mut r,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(hdc, old);
}

impl State {
    unsafe fn new(proxy: Option<String>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let scale = fitting_scale(GetDpiForSystem() as f32 / 96.0);
        let font = |size: i32, weight: i32| {
            CreateFontW(
                -(size as f32 * scale).round() as i32,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                wide("Microsoft YaHei UI").as_ptr(),
            )
        };
        let mut state = Self {
            hwnd: null_mut(),
            path: null_mut(),
            proxy: null_mut(),
            version: null_mut(),
            list: null_mut(),
            progress: null_mut(),
            packs: null_mut(),
            instances: vec![],
            resolving: false,
            directory_dirty: true,
            buttons: vec![],
            rows: vec![],
            root: PathBuf::new(),
            busy: false,
            complete: false,
            status: "自动检测同级 .minecraft，或手动选择文件夹".into(),
            percent: 0,
            width: 960,
            height: 784,
            scale,
            font: font(13, 400),
            small: font(11, 400),
            bold: font(13, 600),
            title: font(24, 700),
            sender,
            receiver,
            cancel: Cancel::default(),
        };
        if let Some(proxy) = proxy {
            state.status = format!("已设置代理：{proxy}");
        }
        state
    }
    unsafe fn edit(&self, text: &str, id: i32) -> HWND {
        let handle = CreateWindowExW(
            0,
            wide("EDIT").as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            self.hwnd,
            id as _,
            GetModuleHandleW(null()),
            null(),
        );
        SendMessageW(handle, WM_SETFONT, self.font as usize, 1);
        handle
    }
    unsafe fn reset_fonts(&mut self) {
        for font in [self.font, self.small, self.bold, self.title] {
            DeleteObject(font);
        }
        let font = |size: i32, weight: i32| {
            CreateFontW(
                -(size as f32 * self.scale).round() as i32,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                wide("Microsoft YaHei UI").as_ptr(),
            )
        };
        self.font = font(13, 400);
        self.small = font(11, 400);
        self.bold = font(13, 600);
        self.title = font(24, 700);
        for handle in [self.path, self.proxy, self.version, self.list, self.packs] {
            if !handle.is_null() {
                SendMessageW(handle, WM_SETFONT, self.font as usize, 1);
            }
        }
        if !self.list.is_null() {
            self.row_height();
        }
    }
    unsafe fn row_height(&self) {
        let height = scale(self, 30);
        let images = ImageList_Create(1, height, ILC_COLOR32, 1, 1);
        if images != 0 {
            let pixels = vec![0u8; height as usize * 4];
            let bitmap = CreateBitmap(1, height, 1, 32, pixels.as_ptr().cast());
            ImageList_Add(images, bitmap, null_mut());
            DeleteObject(bitmap);
            let previous = SendMessageW(
                self.list,
                LVM_SETIMAGELIST,
                LVSIL_SMALL as usize,
                images as isize,
            );
            if previous != 0 {
                ImageList_Destroy(previous as _);
            }
        }
    }
    unsafe fn init(&mut self, proxy: Option<String>) {
        self.path = self.edit("", 201);
        self.proxy = self.edit(proxy.as_deref().unwrap_or(""), 202);
        self.version = self.edit("1.0.0", 203);
        SendMessageW(
            self.path,
            0x1501,
            0,
            wide("自动检测同级 .minecraft，也可选择游戏目录或版本文件夹").as_ptr() as isize,
        );
        SendMessageW(
            self.proxy,
            0x1501,
            0,
            wide("可选，如 127.0.0.1:7897").as_ptr() as isize,
        );
        for (id, label) in [
            (CHOOSE, "手动选择"),
            (AUTO, "自动检测"),
            (SCAN, "开始识别"),
            (OWN, "设为自有"),
            (VERIFY, "下载验证"),
            (EXPORT, "导出清单包"),
        ] {
            let handle = CreateWindowExW(
                0,
                wide("BUTTON").as_ptr(),
                wide(label).as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW as u32,
                0,
                0,
                0,
                0,
                self.hwnd,
                id as _,
                GetModuleHandleW(null()),
                null(),
            );
            self.buttons.push((id, handle));
        }
        self.packs = CreateWindowExW(
            0,
            wide("COMBOBOX").as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
            0,
            0,
            0,
            0,
            self.hwnd,
            PACK as _,
            GetModuleHandleW(null()),
            null(),
        );
        SendMessageW(self.packs, WM_SETFONT, self.font as usize, 1);
        SendMessageW(
            self.packs,
            CB_SETITEMHEIGHT,
            usize::MAX,
            scale(self, 24) as isize,
        );
        SendMessageW(self.packs, CB_SETDROPPEDWIDTH, scale(self, 640) as usize, 0);
        self.list = CreateWindowExW(
            0,
            wide("SysListView32").as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | LVS_REPORT | LVS_SHOWSELALWAYS,
            0,
            0,
            0,
            0,
            self.hwnd,
            204 as _,
            GetModuleHandleW(null()),
            null(),
        );
        SendMessageW(self.list, WM_SETFONT, self.font as usize, 1);
        self.row_height();
        SendMessageW(
            self.list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            0,
            (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_CHECKBOXES) as isize,
        );
        SendMessageW(self.list, LVM_SETBKCOLOR, 0, WHITE as isize);
        SendMessageW(self.list, LVM_SETTEXTBKCOLOR, 0, WHITE as isize);
        SendMessageW(self.list, LVM_SETTEXTCOLOR, 0, INK as isize);
        for (index, label) in ["玩家端安装名", "类型", "识别结果", "可用来源", "版本"]
            .iter()
            .enumerate()
        {
            let mut value = wide(label);
            let mut column: LVCOLUMNW = zeroed();
            column.mask = LVCF_TEXT | LVCF_WIDTH;
            column.pszText = value.as_mut_ptr();
            column.cx = 120;
            SendMessageW(
                self.list,
                LVM_INSERTCOLUMNW,
                index,
                &column as *const _ as isize,
            );
        }
        self.progress = CreateWindowExW(
            0,
            wide("msctls_progress32").as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | PBS_SMOOTH,
            0,
            0,
            0,
            0,
            self.hwnd,
            205 as _,
            GetModuleHandleW(null()),
            null(),
        );
        SendMessageW(self.progress, PBM_SETBARCOLOR, 0, ACCENT as isize);
        SendMessageW(self.progress, PBM_SETBKCOLOR, 0, LINE as isize);
        DragAcceptFiles(self.hwnd, 1);
        SetTimer(self.hwnd, 1, 100, None);
        self.layout();
        if let Err(error) = self.detect_nearby() {
            self.status = format!("自动检测未完成：{error}，请手动选择");
        }
        self.enable();
    }
    unsafe fn layout(&self) {
        let w = self.width;
        let h = self.height;
        let mov = |handle, x, y, width, height| {
            MoveWindow(
                handle,
                scale(self, x),
                scale(self, y),
                scale(self, width),
                scale(self, height),
                1,
            );
        };
        mov(self.path, 50, 148, w - 326, 28);
        mov(self.proxy, w - 280, 228, 228, 22);
        mov(self.packs, 124, 185, w - 296, 240);
        mov(self.version, 124, h - 83, 162, 28);
        for (id, handle) in &self.buttons {
            let (x, y, width) = match *id {
                CHOOSE => (w - 254, 141, 100),
                AUTO => (w - 142, 141, 94),
                SCAN => (w - 142, 183, 94),
                OWN => (w - 450, h - 88, 122),
                VERIFY => (w - 316, h - 88, 122),
                _ => (w - 182, h - 88, 134),
            };
            mov(*handle, x, y, width, 38);
        }
        mov(self.list, 48, 396, w - 96, (h - 592).max(80));
        mov(self.progress, 48, 258, w - 96, 3);
        let available = w - 96;
        let widths = [available - 424, 64, 96, 140, 124];
        for (i, width) in widths.iter().enumerate() {
            SendMessageW(
                self.list,
                LVM_SETCOLUMNWIDTH,
                i,
                scale(self, *width) as isize,
            );
        }
    }
    unsafe fn enable(&self) {
        for (id, handle) in &self.buttons {
            EnableWindow(
                *handle,
                if *id == SCAN {
                    1
                } else if matches!(*id, CHOOSE | AUTO) {
                    (!self.busy) as i32
                } else {
                    (!self.busy && self.complete && !self.rows.is_empty()) as i32
                },
            );
            if *id == SCAN {
                SetWindowTextW(
                    *handle,
                    wide(if self.busy {
                        "取消操作"
                    } else {
                        "开始识别"
                    })
                    .as_ptr(),
                );
            }
        }
        EnableWindow(self.path, (!self.busy) as i32);
        EnableWindow(self.proxy, (!self.busy) as i32);
        EnableWindow(self.version, (!self.busy) as i32);
        EnableWindow(self.list, (!self.busy) as i32);
        EnableWindow(
            self.packs,
            (!self.busy && !self.instances.is_empty()) as i32,
        );
        InvalidateRect(self.hwnd, null(), 0);
    }
    unsafe fn clear_scan(&mut self) {
        self.complete = false;
        self.rows.clear();
        self.percent = 0;
        self.refresh();
        SendMessageW(self.progress, PBM_SETPOS, 0, 0);
    }
    unsafe fn invalidate_directory(&mut self) {
        self.directory_dirty = true;
        self.root.clear();
        self.instances.clear();
        self.clear_scan();
        SendMessageW(self.packs, CB_RESETCONTENT, 0, 0);
        SendMessageW(
            self.packs,
            CB_ADDSTRING,
            0,
            wide("点击开始识别以检测此目录").as_ptr() as isize,
        );
        SendMessageW(self.packs, CB_SETCURSEL, 0, 0);
        self.status = "目录已更改，请检测并选择一个包／版本".into();
        self.enable();
    }
    unsafe fn apply_discovery(&mut self, discovery: Discovery) {
        self.resolving = true;
        SetWindowTextW(
            self.path,
            wide(&instances::display(&discovery.location)).as_ptr(),
        );
        self.directory_dirty = false;
        self.clear_scan();
        self.instances = discovery.instances;
        self.root.clear();
        SendMessageW(self.packs, CB_RESETCONTENT, 0, 0);
        SendMessageW(
            self.packs,
            CB_ADDSTRING,
            0,
            wide(if self.instances.is_empty() {
                "未发现游戏包，请选择具体文件夹"
            } else {
                "请选择一个包／版本"
            })
            .as_ptr() as isize,
        );
        for instance in &self.instances {
            let layout = if instance.info.layout == "version-isolated" {
                "版本隔离"
            } else {
                "独立目录"
            };
            let label = format!("{}  ·  {layout}", instance.label);
            SendMessageW(self.packs, CB_ADDSTRING, 0, wide(&label).as_ptr() as isize);
        }
        SendMessageW(
            self.packs,
            CB_SETCURSEL,
            if self.instances.len() == 1 { 1 } else { 0 },
            0,
        );
        self.resolving = false;
        self.pack_changed();
    }
    unsafe fn detect_nearby(&mut self) -> Result<()> {
        if let Some(discovery) = instances::automatic()? {
            self.apply_discovery(discovery);
        } else {
            let executable = std::env::current_exe()?;
            self.apply_discovery(Discovery {
                location: executable.parent().unwrap().to_path_buf(),
                instances: vec![],
            });
            self.status = "将工具放到 .minecraft 同级，或点击手动选择".into();
        }
        Ok(())
    }
    unsafe fn choose_location(&mut self, folder: &Path) -> Result<()> {
        self.apply_discovery(instances::discover(folder)?);
        Ok(())
    }
    unsafe fn current_pack(&self) -> Result<&Instance> {
        let index = SendMessageW(self.packs, CB_GETCURSEL, 0, 0);
        if index <= 0 {
            bail!("请先从下拉列表选择一个包／版本");
        }
        self.instances
            .get(index as usize - 1)
            .ok_or_else(|| anyhow::anyhow!("所选包已失效，请重新检测"))
    }
    unsafe fn pack_changed(&mut self) {
        self.clear_scan();
        self.status = if let Ok(instance) = self.current_pack() {
            let root = instance.root.clone();
            let id = instance.info.id.clone();
            self.root = root;
            format!("已选择 {id}，仅扫描此包自己的资源")
        } else if self.instances.is_empty() {
            "未发现游戏包，请手动选择文件夹".into()
        } else {
            format!("检测到 {} 个包，请选择一个版本", self.instances.len())
        };
        self.enable();
    }
    unsafe fn selected(&mut self) {
        for (index, row) in self.rows.iter_mut().enumerate() {
            row.selected = (SendMessageW(
                self.list,
                LVM_GETITEMSTATE,
                index,
                LVIS_STATEIMAGEMASK as isize,
            ) >> 12)
                == 2;
        }
    }
    unsafe fn refresh(&self) {
        ShowWindow(
            self.list,
            if self.rows.is_empty() {
                SW_HIDE
            } else {
                SW_SHOW
            },
        );
        SendMessageW(self.list, LVM_DELETEALLITEMS, 0, 0);
        for index in 0..self.rows.len() {
            self.insert_row(index);
        }
    }
    unsafe fn insert_row(&self, index: usize) {
        let row = &self.rows[index];
        let sources = if row.own {
            "自有文件".into()
        } else {
            row.resource
                .sources
                .iter()
                .map(|s| {
                    if s.provider == "modrinth" {
                        "Modrinth"
                    } else {
                        "CurseForge"
                    }
                })
                .collect::<Vec<_>>()
                .join(" / ")
        };
        let kind = match row.resource.kind.as_str() {
            "mod" => "Mod",
            "resourcepack" => "材质",
            _ => "光影",
        };
        let version = if row.own {
            "随本次发布"
        } else {
            row.resource
                .sources
                .first()
                .map(|s| s.version.as_str())
                .unwrap_or("—")
        };
        let mut name = wide(&row.resource.filename);
        let mut item: LVITEMW = zeroed();
        item.mask = LVIF_TEXT;
        item.iItem = index as i32;
        item.pszText = name.as_mut_ptr();
        SendMessageW(self.list, LVM_INSERTITEMW, 0, &item as *const _ as isize);
        for (sub, value) in [
            kind,
            if row.own { "自有资源" } else { &row.status },
            &sources,
            version,
        ]
        .iter()
        .enumerate()
        {
            let mut value = wide(value);
            item.iSubItem = sub as i32 + 1;
            item.pszText = value.as_mut_ptr();
            SendMessageW(
                self.list,
                LVM_SETITEMTEXTW,
                index,
                &item as *const _ as isize,
            );
        }
        item.stateMask = LVIS_STATEIMAGEMASK;
        item.state = if row.selected { 2 << 12 } else { 1 << 12 };
        SendMessageW(
            self.list,
            LVM_SETITEMSTATE,
            index,
            &item as *const _ as isize,
        );
    }
    unsafe fn start_scan(&mut self) -> Result<()> {
        if self.busy {
            self.cancel.stop();
            self.status = "正在取消，当前网络请求结束后停止…".into();
            return Ok(());
        }
        if self.directory_dirty {
            self.choose_location(Path::new(get_text(self.path).trim()))?;
        }
        self.root = self.current_pack()?.root.clone();
        core::files_in(&self.root)?;
        self.rows.clear();
        self.refresh();
        self.complete = false;
        self.busy = true;
        self.percent = 0;
        self.cancel = Cancel::default();
        self.status = "正在扫描资源并计算哈希…".into();
        self.enable();
        let root = self.root.clone();
        let client = Client {
            proxy: proxy(self.proxy),
        };
        let cancel = self.cancel.clone();
        let tx = self.sender.clone();
        std::thread::spawn(move || {
            let result = core::scan(&root, &client, &cancel, |done, total, row| {
                let _ = tx.send(Event::Row(row, done, total));
            })
            .map(|rows| format!("识别完成，共 {} 个资源。勾选需要导出的文件。", rows.len()));
            let _ = tx.send(Event::End(result));
        });
        Ok(())
    }
    unsafe fn mark_own(&mut self) -> Result<()> {
        self.selected();
        let index = SendMessageW(
            self.list,
            LVM_GETNEXTITEM,
            usize::MAX,
            LVNI_SELECTED as isize,
        );
        if index < 0 {
            bail!("请先在列表中选中一个资源");
        }
        let row = &mut self.rows[index as usize];
        if row.resource.size == 0 {
            bail!("此文件扫描失败，请修复后重新识别");
        }
        row.own = !row.own;
        self.status = if row.own {
            "已标记为自有资源，导出包会包含此文件"
        } else {
            "已恢复为第三方来源识别结果"
        }
        .into();
        self.refresh();
        InvalidateRect(self.hwnd, null(), 0);
        Ok(())
    }
    unsafe fn export_or_verify(&mut self, verify: bool) -> Result<()> {
        self.selected();
        let mut manifest = core::manifest(&self.rows, &get_text(self.version))?;
        manifest.instance = Some(self.current_pack()?.info.clone());
        let selected = if verify {
            choose_folder(self.hwnd)?
        } else {
            save_package(self.hwnd, &manifest.version)?
        };
        let Some(selected) = selected else {
            return Ok(());
        };
        let root = self.root.clone();
        let client = Client {
            proxy: proxy(self.proxy),
        };
        let tx = self.sender.clone();
        let cancel = Cancel::default();
        self.cancel = cancel.clone();
        self.busy = true;
        self.percent = 0;
        self.status = if verify {
            "正在导出并重新下载验证…"
        } else {
            "正在校验文件并生成导出包…"
        }
        .into();
        self.enable();
        std::thread::spawn(move || {
            let result = (|| -> Result<String> {
                if verify {
                    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
                    let package = selected.join(format!("HotCoco-验证-{stamp}.zip"));
                    let destination = selected.join(format!("HotCoco-验证-{stamp}"));
                    core::export(&root, &manifest, &package, &cancel)?;
                    core::restore(
                        &package,
                        &destination,
                        &client,
                        None,
                        &cancel,
                        |done, total, path| {
                            let _ = tx.send(Event::Progress(
                                format!("已验证 {done}/{total}：{path}"),
                                done * 100 / total,
                            ));
                        },
                    )?;
                    Ok(format!(
                        "下载验证通过，安装名与哈希一致。结果：{}",
                        destination.display()
                    ))
                } else {
                    core::export(&root, &manifest, &selected, &cancel)?;
                    Ok(format!("导出成功：{}", selected.display()))
                }
            })();
            let _ = tx.send(Event::End(result));
        });
        Ok(())
    }
    unsafe fn poll(&mut self) {
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                Event::Row(row, done, total) => {
                    self.status = format!("识别中 {done}/{total}：{}", row.resource.filename);
                    self.rows.push(row);
                    self.percent = done * 100 / total;
                    ShowWindow(self.list, SW_SHOW);
                    self.insert_row(self.rows.len() - 1);
                }
                Event::Progress(message, percent) => {
                    self.status = message;
                    self.percent = percent;
                }
                Event::End(result) => {
                    self.busy = false;
                    match result {
                        Ok(message) => {
                            self.status = message;
                            self.complete = true;
                            self.percent = 100;
                        }
                        Err(e) => {
                            self.status = format!("未完成：{e}");
                        }
                    }
                    self.enable();
                }
            }
            SendMessageW(self.progress, PBM_SETPOS, self.percent, 0);
            InvalidateRect(self.hwnd, null(), 0);
        }
    }
}

unsafe fn get_text(hwnd: HWND) -> String {
    let mut value = vec![0u16; GetWindowTextLengthW(hwnd) as usize + 1];
    GetWindowTextW(hwnd, value.as_mut_ptr(), value.len() as i32);
    String::from_utf16_lossy(&value[..value.len() - 1])
}
unsafe fn proxy(hwnd: HWND) -> Option<String> {
    let value = get_text(hwnd);
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

unsafe fn choose_folder(hwnd: HWND) -> Result<Option<PathBuf>> {
    use windows::{
        core::Interface,
        Win32::{System::Com::*, UI::Shell::*},
    };
    let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
    dialog.SetOptions(
        dialog.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST,
    )?;
    if let Err(e) = dialog.Show(windows::Win32::Foundation::HWND(hwnd)) {
        if e.code().0 as u32 == 0x800704c7 {
            return Ok(None);
        }
        return Err(e.into());
    }
    let item = dialog.GetResult()?;
    let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
    let result = PathBuf::from(name.to_string()?);
    CoTaskMemFree(Some(name.0.cast()));
    let _ = dialog.as_raw();
    Ok(Some(result))
}
unsafe fn save_package(hwnd: HWND, version: &str) -> Result<Option<PathBuf>> {
    use windows::{
        core::HSTRING,
        Win32::{System::Com::*, UI::Shell::*},
    };
    let dialog: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?;
    let safe: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    dialog.SetFileName(&HSTRING::from(format!("HotCoco-{safe}.hotcoco.zip")))?;
    dialog.SetDefaultExtension(&HSTRING::from("zip"))?;
    if let Err(e) = dialog.Show(windows::Win32::Foundation::HWND(hwnd)) {
        if e.code().0 as u32 == 0x800704c7 {
            return Ok(None);
        }
        return Err(e.into());
    }
    let item = dialog.GetResult()?;
    let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
    let result = PathBuf::from(name.to_string()?);
    CoTaskMemFree(Some(name.0.cast()));
    Ok(Some(result))
}

unsafe fn paint(hdc: HDC, s: &State, preview: bool) {
    let w = s.width;
    let h = s.height;
    let background = CreateSolidBrush(BG);
    FillRect(hdc, &rect(s, 0, 0, w, h), background);
    DeleteObject(background);
    card(hdc, s, 28, 26, 46, 46, 0x4a74f2, 0x4a74f2, 16);
    text(hdc, s, "HC", 37, 33, 34, 30, s.bold, WHITE);
    text(hdc, s, "资源导出", 90, 19, 420, 38, s.title, INK);
    text(
        hdc,
        s,
        "识别来源 · 保留命名 · 轻量发布",
        92,
        58,
        580,
        24,
        s.small,
        MUTED,
    );
    card(hdc, s, w - 182, 31, 154, 30, 0xf1e8f0, 0xf1e8f0, 22);
    text(
        hdc,
        s,
        "HOTCOCO   /   0.2.0",
        w - 169,
        32,
        130,
        27,
        s.small,
        ACCENT,
    );
    card(hdc, s, 28, 96, w - 56, 170, WHITE, LINE, 20);
    text(hdc, s, "游戏目录", 48, 108, 250, 25, s.bold, INK);
    card(hdc, s, 46, 141, w - 318, 38, WHITE, LINE, 10);
    text(hdc, s, "选择包", 48, 183, 72, 34, s.bold, INK);
    card(hdc, s, 118, 181, w - 284, 38, WHITE, LINE, 10);
    text(hdc, s, &s.status, 48, 228, w - 402, 23, s.small, MUTED);
    text(hdc, s, "代理", w - 324, 228, 42, 23, s.small, MUTED);
    let ready = s
        .rows
        .iter()
        .filter(|r| !r.own && !r.resource.sources.is_empty())
        .count();
    let own = s.rows.iter().filter(|r| r.own).count();
    let pending = s.rows.len() - ready - own;
    let cw = (w - 92) / 4;
    for (i, label, value, color) in [
        (0, "资源总数", s.rows.len(), INK),
        (1, "已识别", ready, 0x6d9360),
        (2, "自有文件", own, ACCENT),
        (3, "待处理", pending, 0x4c93c7),
    ] {
        let x = 28 + i * (cw + 12);
        card(hdc, s, x, 278, cw, 62, WHITE, LINE, 16);
        text(hdc, s, label, x + 17, 287, cw - 70, 21, s.small, MUTED);
        text(
            hdc,
            s,
            &value.to_string(),
            x + 17,
            305,
            cw - 40,
            27,
            s.bold,
            color,
        );
    }
    card(hdc, s, 28, 352, w - 56, h - 526, WHITE, LINE, 20);
    text(hdc, s, "资源清单", 48, 361, 200, 26, s.bold, INK);
    text(
        hdc,
        s,
        "勾选需要导出的文件",
        w - 264,
        361,
        216,
        26,
        s.small,
        MUTED,
    );
    if s.rows.is_empty() {
        text(
            hdc,
            s,
            "把测试好的资源交给清单",
            64,
            444,
            w - 128,
            32,
            s.bold,
            INK,
        );
        text(
            hdc,
            s,
            "扫描 Mod、材质和光影，按文件内容匹配具体版本。",
            64,
            483,
            w - 128,
            25,
            s.font,
            MUTED,
        );
        text(
            hdc,
            s,
            "你修改过的文件名，会原样部署到玩家本地。",
            64,
            513,
            w - 128,
            25,
            s.font,
            MUTED,
        );
    }
    let selected = if preview {
        Some(0)
    } else {
        let n = SendMessageW(s.list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
        if n >= 0 {
            Some(n as usize)
        } else {
            None
        }
    };
    if let Some(row) = selected.and_then(|i| s.rows.get(i)) {
        let detail = if row.note.is_empty() {
            format!("部署路径：{}", row.resource.path)
        } else {
            row.note.clone()
        };
        text(hdc, s, &detail, 40, h - 164, w - 80, 26, s.small, MUTED);
        let original = row
            .resource
            .sources
            .first()
            .map(|src| format!("上游：{}  ·  安装：{}", src.filename, row.resource.filename))
            .unwrap_or_else(|| format!("部署路径：{}", row.resource.path));
        text(hdc, s, &original, 40, h - 141, w - 80, 22, s.small, MUTED);
    } else {
        text(
            hdc,
            s,
            &if s.root.as_os_str().is_empty() {
                "选择一个版本后，只扫描该包自己的资源目录。".into()
            } else {
                format!("当前包资源目录：{}", instances::display(&s.root))
            },
            40,
            h - 158,
            w - 80,
            30,
            s.small,
            MUTED,
        );
    }
    card(hdc, s, 28, h - 104, w - 56, 82, WHITE, LINE, 18);
    text(hdc, s, "发布版本", 48, h - 87, 72, 30, s.bold, INK);
    card(hdc, s, 118, h - 89, 176, 38, WHITE, LINE, 10);
    text(
        hdc,
        s,
        "导出后会再次核对本地文件，确保清单与文件内容一致。",
        48,
        h - 48,
        w - 96,
        22,
        s.small,
        MUTED,
    );
    if preview {
        text(
            hdc,
            s,
            "D:\\HotCoco\\.minecraft",
            51,
            144,
            w - 330,
            32,
            s.font,
            INK,
        );
        text(
            hdc,
            s,
            "使用系统网络设置",
            w - 278,
            228,
            230,
            22,
            s.small,
            MUTED,
        );
        text(hdc, s, "1.0.0", 125, h - 85, 156, 30, s.font, INK);
        text(
            hdc,
            s,
            "HotCoco-1.21.1  ·  版本隔离",
            126,
            185,
            w - 310,
            30,
            s.font,
            INK,
        );
        for (id, label) in [
            (CHOOSE, "手动选择"),
            (AUTO, "自动检测"),
            (SCAN, "开始识别"),
            (OWN, "设为自有"),
            (VERIFY, "下载验证"),
            (EXPORT, "导出清单包"),
        ] {
            let (x, y, bw) = match id {
                CHOOSE => (w - 254, 141, 100),
                AUTO => (w - 142, 141, 94),
                SCAN => (w - 142, 183, 94),
                OWN => (w - 450, h - 88, 122),
                VERIFY => (w - 316, h - 88, 122),
                _ => (w - 182, h - 88, 134),
            };
            button(
                hdc,
                s,
                rect(s, x, y, bw, 38),
                label,
                matches!(id, SCAN | EXPORT),
                s.rows.is_empty() && matches!(id, OWN | VERIFY | EXPORT),
                false,
            );
        }
        card(hdc, s, 48, 396, w - 96, 29, 0xfaf7f5, 0xfaf7f5, 1);
        for (label, x, bw) in [
            ("玩家端安装名", 76, w - 520),
            ("类型", w - 472, 64),
            ("识别结果", w - 408, 96),
            ("可用来源", w - 312, 140),
            ("版本", w - 172, 124),
        ] {
            text(hdc, s, label, x, 397, bw, 27, s.small, MUTED);
        }
        for (i, row) in s.rows.iter().enumerate() {
            let y = 432 + i as i32 * 33;
            card(hdc, s, 55, y + 8, 13, 13, ACCENT, ACCENT, 4);
            text(hdc, s, "✓", 56, y + 5, 15, 18, s.small, WHITE);
            text(
                hdc,
                s,
                &row.resource.filename,
                76,
                y,
                w - 552,
                30,
                s.font,
                INK,
            );
            let kind = match row.resource.kind.as_str() {
                "mod" => "Mod",
                "resourcepack" => "材质",
                _ => "光影",
            };
            text(hdc, s, kind, w - 472, y, 64, 30, s.small, MUTED);
            text(
                hdc,
                s,
                if row.own { "自有资源" } else { "已识别" },
                w - 408,
                y,
                96,
                30,
                s.small,
                if row.own { ACCENT } else { 0x6d9360 },
            );
            text(
                hdc,
                s,
                if row.own {
                    "随包导出"
                } else {
                    "Modrinth / CF"
                },
                w - 312,
                y,
                140,
                30,
                s.small,
                MUTED,
            );
            text(
                hdc,
                s,
                row.resource
                    .sources
                    .first()
                    .map(|s| s.version.as_str())
                    .unwrap_or("本次发布"),
                w - 172,
                y,
                124,
                30,
                s.small,
                MUTED,
            );
        }
        let color = if s.rows.is_empty() { LINE } else { ACCENT };
        card(hdc, s, 48, 258, w - 96, 3, color, color, 1);
    }
}

unsafe extern "system" fn procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    if pointer.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let s = &mut *pointer;
    match message {
        WM_SIZE => {
            let mut area: RECT = zeroed();
            GetClientRect(hwnd, &mut area);
            s.width = (area.right as f32 / s.scale) as i32;
            s.height = (area.bottom as f32 / s.scale) as i32;
            s.layout();
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_GETMINMAXINFO => {
            let info = &mut *(lparam as *mut MINMAXINFO);
            info.ptMinTrackSize.x = scale(s, 920);
            info.ptMinTrackSize.y = scale(s, 764);
            0
        }
        WM_DPICHANGED => {
            s.scale = fitting_scale((wparam & 0xffff) as f32 / 96.0);
            s.reset_fonts();
            let r = &*(lparam as *const RECT);
            SetWindowPos(
                hwnd,
                null_mut(),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            s.layout();
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let memory = CreateCompatibleDC(hdc);
            let bitmap = CreateCompatibleBitmap(hdc, scale(s, s.width), scale(s, s.height));
            let old = SelectObject(memory, bitmap);
            paint(memory, s, false);
            BitBlt(
                hdc,
                0,
                0,
                scale(s, s.width),
                scale(s, s.height),
                memory,
                0,
                0,
                SRCCOPY,
            );
            SelectObject(memory, old);
            DeleteObject(bitmap);
            DeleteDC(memory);
            EndPaint(hwnd, &ps);
            0
        }
        WM_CTLCOLOREDIT => {
            SetBkColor(wparam as HDC, WHITE);
            SetTextColor(wparam as HDC, INK);
            GetStockObject(WHITE_BRUSH) as isize
        }
        WM_DRAWITEM => {
            let item = &*(lparam as *const DRAWITEMSTRUCT);
            button(
                item.hDC,
                s,
                item.rcItem,
                &get_text(item.hwndItem),
                matches!(item.CtlID as i32, SCAN | EXPORT),
                item.itemState & ODS_DISABLED != 0,
                item.itemState & ODS_SELECTED != 0,
            );
            if item.itemState & ODS_FOCUS != 0 {
                let mut r = item.rcItem;
                InflateRect(&mut r, -4, -4);
                DrawFocusRect(item.hDC, &r);
            }
            0
        }
        WM_TIMER => {
            s.poll();
            0
        }
        WM_NOTIFY => {
            let header = &*(lparam as *const NMHDR);
            if header.hwndFrom == s.list {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xffff) as i32;
            if id == 201 && (wparam >> 16) as u32 == EN_CHANGE && !s.busy && !s.resolving {
                s.invalidate_directory();
            }
            if id == PACK && (wparam >> 16) as u32 == CBN_SELCHANGE && !s.busy && !s.resolving {
                s.pack_changed();
            }
            let result = match id {
                CHOOSE => choose_folder(hwnd).and_then(|folder| match folder {
                    Some(folder) => s.choose_location(&folder),
                    None => Ok(()),
                }),
                AUTO => s.detect_nearby(),
                SCAN => s.start_scan(),
                OWN => s.mark_own(),
                EXPORT => s.export_or_verify(false),
                VERIFY => s.export_or_verify(true),
                _ => Ok(()),
            };
            if let Err(e) = result {
                MessageBoxW(
                    hwnd,
                    wide(&e.to_string()).as_ptr(),
                    wide("HotCoco 资源导出").as_ptr(),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_DROPFILES => {
            let drop = wparam as _;
            if !s.busy {
                let size = DragQueryFileW(drop, 0, null_mut(), 0);
                let mut path = vec![0u16; size as usize + 1];
                DragQueryFileW(drop, 0, path.as_mut_ptr(), size + 1);
                let folder = PathBuf::from(String::from_utf16_lossy(&path[..size as usize]));
                if let Err(error) = s.choose_location(&folder) {
                    s.status = format!("未检测到游戏包：{error}");
                    InvalidateRect(hwnd, null(), 0);
                }
            }
            DragFinish(drop);
            0
        }
        WM_DESTROY => {
            s.cancel.stop();
            KillTimer(hwnd, 1);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

pub fn run(proxy: Option<String>) -> Result<()> {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let controls = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_PROGRESS_CLASS,
        };
        InitCommonControlsEx(&controls);
        let instance = GetModuleHandleW(null());
        let class_name = wide("HotCocoExporterWindow");
        let mut class: WNDCLASSW = zeroed();
        class.lpfnWndProc = Some(procedure);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        class.hIcon = LoadIconW(instance, 1 as _);
        RegisterClassW(&class);
        let mut state = Box::new(State::new(proxy.clone()));
        let mut area = rect(&state, 0, 0, state.width, state.height);
        AdjustWindowRectEx(&mut area, WS_OVERLAPPEDWINDOW, 0, 0);
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("HotCoco · 资源导出工具").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            area.right - area.left,
            area.bottom - area.top,
            null_mut(),
            null_mut(),
            instance,
            (&mut *state as *mut State).cast(),
        );
        if hwnd.is_null() {
            bail!("无法创建窗口：{}", GetLastError());
        }
        state.hwnd = hwnd;
        state.init(proxy);
        state.refresh();
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        let mut message: MSG = zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        for font in [state.font, state.small, state.bold, state.title] {
            DeleteObject(font);
        }
        Ok(())
    }
}

/// Render the same layout into a memory bitmap; this never creates or captures a desktop window.
pub fn preview(path: &Path, empty: bool) -> Result<()> {
    unsafe {
        let mut s = State::new(None);
        s.scale = 1.0;
        s.reset_fonts();
        s.status = "识别完成，共 5 个资源。自定义文件名将原样保留。".into();
        s.root = PathBuf::from(r"D:\HotCoco\.minecraft\versions\HotCoco-1.21.1");
        for (name, kind, version) in [
            ("[性能优化] 钠 Sodium.jar", "mod", "0.8.13"),
            ("Fabric API 基础库.jar", "mod", "0.116.17"),
            ("[物品查询] JEI.jar", "mod", "19.64.0"),
            ("鲜活动画 材质包.zip", "resourcepack", "1.11.1"),
            ("Complementary 自定义光影.zip", "shader", "r5.9.3"),
        ] {
            let source = crate::core::Source {
                provider: "modrinth".into(),
                project_id: "preview".into(),
                version_id: "preview".into(),
                version: version.into(),
                filename: if kind == "mod" {
                    "original-mod.jar"
                } else {
                    "original-pack.zip"
                }
                .into(),
                url: String::new(),
            };
            s.rows.push(Row {
                resource: crate::core::Resource {
                    resource_id: String::new(),
                    path: format!("mods/{name}"),
                    filename: name.into(),
                    kind: kind.into(),
                    size: 0,
                    hashes: Default::default(),
                    sources: vec![source],
                    embedded: None,
                },
                selected: true,
                own: false,
                status: "已识别".into(),
                note: "保留自定义文件名，匹配上游具体版本".into(),
            });
        }
        if empty {
            s.rows.clear();
            s.status = "选择实例目录，或把文件夹拖到窗口中".into();
        }
        let dc = CreateCompatibleDC(null_mut());
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = s.width;
        info.bmiHeader.biHeight = -s.height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut pixels = null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
        if bitmap.is_null() {
            bail!("无法创建离屏预览");
        }
        let old = SelectObject(dc, bitmap);
        paint(dc, &s, true);
        GdiFlush();
        let raw = std::slice::from_raw_parts(
            pixels as *const u8,
            s.width as usize * s.height as usize * 4,
        );
        let mut rgb = Vec::with_capacity(s.width as usize * s.height as usize * 3);
        for pixel in raw.chunks_exact(4) {
            rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
        let mut encoder = png::Encoder::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?,
            s.width as u32,
            s.height as u32,
        );
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&rgb)?;
        SelectObject(dc, old);
        DeleteObject(bitmap);
        DeleteDC(dc);
        for font in [s.font, s.small, s.bold, s.title] {
            DeleteObject(font);
        }
        Ok(())
    }
}
