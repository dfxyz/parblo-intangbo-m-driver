use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Error, Result, anyhow};
use evdev_rs::enums::EV_KEY;
use nix::errno::Errno;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::inotify::{self, Inotify, InotifyEvent};
use serde::Deserialize;

use crate::cancel::CancelToken;
use crate::error;
use crate::warn;

macro_rules! try_into {
    ($value: ident => $($field:ident),+ $(,)?) => {
        Ok(Self {
            $(
                $field: $value.$field.try_into().context(concat!("转换字段'", stringify!($field), "'时发生错误"))?
            ),+
        })
    };
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawConfig {
    // X轴最大值
    x_max_value: Option<u16>,

    // Y轴最大值
    y_max_value: Option<u16>,

    // X轴的比例映射
    x_map: Option<(f32, f32)>,

    // Y轴的比例映射
    y_map: Option<(f32, f32)>,

    // 按键映射配置方案
    #[serde(rename = "keymap")]
    keymaps: Vec<RawKeymapConfig>,
}
#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawKeymapConfig {
    button0: String,
    button1: String,
    button2: String,
    button3: String,
    button4: String,
    button5: String,
    button6: String,
    button7: String,
    ring0: String,
    ring1: String,
    ring_button: String,
}
impl Default for RawKeymapConfig {
    fn default() -> Self {
        macro_rules! default_fallback {
            ($($field:ident),+ $(,)?) => {
                Self {
                    $(
                        $field: "fallback".to_string(),
                    )+
                }
            };
        }
        default_fallback! {
            button0, button1, button2, button3, button4, button5, button6, button7,
            ring0, ring1, ring_button,
        }
    }
}

#[derive(Clone)]
enum ImmediateKeymap {
    None,
    Press(Arc<Vec<EV_KEY>>),
    SwitchSchema,
    Fallback,
}
impl TryFrom<String> for ImmediateKeymap {
    type Error = Error;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        let iter = value.split("+").map(|s| s.trim());
        let mut parts = Vec::new();
        for part in iter {
            if !parts.contains(&part) {
                parts.push(part);
            }
        }
        if parts.contains(&"switchSchema") {
            if parts.len() > 1 {
                return Err(anyhow!("不能把'switchSchema'和其他键组合"));
            }
            return Ok(ImmediateKeymap::SwitchSchema);
        }
        if parts.contains(&"fallback") {
            if parts.len() > 1 {
                return Err(anyhow!("不能把'fallback'和其他键组合"));
            }
            return Ok(ImmediateKeymap::Fallback);
        }
        if parts.contains(&"none") {
            if parts.len() > 1 {
                return Err(anyhow!("不能把'none'和其他键组合"));
            }
            return Ok(ImmediateKeymap::None);
        }

        let mut codes = Vec::with_capacity(parts.len());
        macro_rules! match_key {
            ($value:ident, { $($key:literal => $code:expr),+ $(,)? }) => {
                match $value {
                    $(x if x == $key => {
                        codes.push($code);
                    }),+
                    _ => {
                        return Err(anyhow!("'{}'不是有效的按键映射配置", $value));
                    }
                }
            };
        }
        for part in parts {
            match_key!(part, {
                // Letters
                "a" => EV_KEY::KEY_A, "b" => EV_KEY::KEY_B, "c" => EV_KEY::KEY_C, "d" => EV_KEY::KEY_D,
                "e" => EV_KEY::KEY_E, "f" => EV_KEY::KEY_F, "g" => EV_KEY::KEY_G, "h" => EV_KEY::KEY_H,
                "i" => EV_KEY::KEY_I, "j" => EV_KEY::KEY_J, "k" => EV_KEY::KEY_K, "l" => EV_KEY::KEY_L,
                "m" => EV_KEY::KEY_M, "n" => EV_KEY::KEY_N, "o" => EV_KEY::KEY_O, "p" => EV_KEY::KEY_P,
                "q" => EV_KEY::KEY_Q, "r" => EV_KEY::KEY_R, "s" => EV_KEY::KEY_S, "t" => EV_KEY::KEY_T,
                "u" => EV_KEY::KEY_U, "v" => EV_KEY::KEY_V, "w" => EV_KEY::KEY_W, "x" => EV_KEY::KEY_X,
                "y" => EV_KEY::KEY_Y, "z" => EV_KEY::KEY_Z,
                // Numbers
                "0" => EV_KEY::KEY_0, "1" => EV_KEY::KEY_1, "2" => EV_KEY::KEY_2, "3" => EV_KEY::KEY_3,
                "4" => EV_KEY::KEY_4, "5" => EV_KEY::KEY_5, "6" => EV_KEY::KEY_6, "7" => EV_KEY::KEY_7,
                "8" => EV_KEY::KEY_8, "9" => EV_KEY::KEY_9,
                // Symbols
                "-" => EV_KEY::KEY_MINUS, "=" => EV_KEY::KEY_EQUAL, "\\" => EV_KEY::KEY_BACKSLASH,
                "`" => EV_KEY::KEY_GRAVE, "[" => EV_KEY::KEY_LEFTBRACE, "]" => EV_KEY::KEY_RIGHTBRACE,
                ";" => EV_KEY::KEY_SEMICOLON, "'" => EV_KEY::KEY_APOSTROPHE, "," => EV_KEY::KEY_COMMA,
                "." => EV_KEY::KEY_DOT, "/" => EV_KEY::KEY_SLASH,
                // Special keys
                "esc" => EV_KEY::KEY_ESC, "tab" => EV_KEY::KEY_TAB, "backspace" => EV_KEY::KEY_BACKSPACE,
                "enter" => EV_KEY::KEY_ENTER, "space" => EV_KEY::KEY_SPACE, "home" => EV_KEY::KEY_HOME,
                "end" => EV_KEY::KEY_END, "pageup" => EV_KEY::KEY_PAGEUP, "pagedown" => EV_KEY::KEY_PAGEDOWN,
                "insert" => EV_KEY::KEY_INSERT, "delete" => EV_KEY::KEY_DELETE,
                // Modifier keys
                "ctrl" => EV_KEY::KEY_LEFTCTRL, "shift" => EV_KEY::KEY_LEFTSHIFT,
                "alt" => EV_KEY::KEY_LEFTALT, "meta" => EV_KEY::KEY_LEFTMETA,
            });
        }
        Ok(ImmediateKeymap::Press(Arc::new(codes)))
    }
}

struct ImmediateKeymapConfig {
    button0: ImmediateKeymap,
    button1: ImmediateKeymap,
    button2: ImmediateKeymap,
    button3: ImmediateKeymap,
    button4: ImmediateKeymap,
    button5: ImmediateKeymap,
    button6: ImmediateKeymap,
    button7: ImmediateKeymap,
    ring0: ImmediateKeymap,
    ring1: ImmediateKeymap,
    ring_button: ImmediateKeymap,
}
impl TryFrom<RawKeymapConfig> for ImmediateKeymapConfig {
    type Error = anyhow::Error;
    fn try_from(value: RawKeymapConfig) -> Result<Self> {
        try_into! { value =>
            button0, button1, button2, button3, button4, button5, button6, button7,
            ring0, ring1, ring_button,
        }
    }
}
impl ImmediateKeymapConfig {
    fn resolve(&mut self, other: &Self) {
        macro_rules! resolve {
            ($($field:ident),+ $(,)?) => {
                $(
                    if let ImmediateKeymap::Fallback = self.$field {
                        self.$field = other.$field.clone();
                    }
                )+
            };
        }
        resolve! {
            button0, button1, button2, button3, button4, button5, button6, button7,
            ring0, ring1, ring_button,
        }
    }
}

#[derive(Clone, Default)]
pub struct Config {
    pub x_max_value: u16,
    pub y_max_value: u16,
    pub x_map: Option<(f32, f32)>,
    pub y_map: Option<(f32, f32)>,
    pub keymaps: Vec<KeymapConfig>,
}
#[derive(Clone, Default)]
pub struct KeymapConfig {
    pub button0: Keymap,
    pub button1: Keymap,
    pub button2: Keymap,
    pub button3: Keymap,
    pub button4: Keymap,
    pub button5: Keymap,
    pub button6: Keymap,
    pub button7: Keymap,
    pub ring0: Keymap,
    pub ring1: Keymap,
    pub ring_button: Keymap,
}
#[derive(Clone, Default)]
pub enum Keymap {
    #[default]
    None,
    Press(Arc<Vec<EV_KEY>>),
    SwitchSchema,
}
impl TryFrom<ImmediateKeymap> for Keymap {
    type Error = Error;
    fn try_from(value: ImmediateKeymap) -> Result<Self> {
        match value {
            ImmediateKeymap::Press(codes) => Ok(Self::Press(codes.clone())),
            ImmediateKeymap::SwitchSchema => Ok(Self::SwitchSchema),
            ImmediateKeymap::Fallback => Ok(Self::None),
            ImmediateKeymap::None => Ok(Self::None),
        }
    }
}
impl TryFrom<ImmediateKeymapConfig> for KeymapConfig {
    type Error = Error;
    fn try_from(value: ImmediateKeymapConfig) -> Result<Self> {
        try_into! { value =>
            button0, button1, button2, button3, button4, button5, button6, button7,
            ring0, ring1, ring_button,
        }
    }
}
impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path).context("")?;
        let raw: RawConfig = toml::from_str(&content).context("TOML解析失败")?;
        if raw.keymaps.is_empty() {
            return Err(anyhow!("没有配置keymap"));
        }

        let iter = raw.keymaps.into_iter().map(|x| {
            ImmediateKeymapConfig::try_from(x)
                .context("无法把原始按键映射配置转换成中间形态的按键映射配置")
        });
        let mut prev = None;
        let mut immediate_keymaps = vec![];
        for result in iter {
            let mut keymap = result?;
            if let Some(prev) = prev {
                keymap.resolve(prev);
            }
            immediate_keymaps.push(keymap);
            prev = immediate_keymaps.last();
        }

        let mut keymaps = vec![];
        for keymap in immediate_keymaps {
            keymaps.push(
                KeymapConfig::try_from(keymap)
                    .context("无法把中间形态的按键映射配置转换成最终形态的按键映射配置")?,
            );
        }

        macro_rules! check_map_values {
            ($field:ident) => {
                if let Some((min, max)) = raw.$field {
                    if !(0f32..=1f32).contains(&min) {
                        return Err(anyhow!(concat!(
                            stringify!($field),
                            "的最小值必须在0到1之间"
                        )));
                    }
                    if !(0f32..=1f32).contains(&max) {
                        return Err(anyhow!(concat!(
                            stringify!($field),
                            "的最大值必须在0到1之间"
                        )));
                    }
                    if min >= max {
                        return Err(anyhow!(concat!(
                            stringify!($field),
                            "的最小值必须小于最大值"
                        )));
                    }
                }
            };
        }
        check_map_values!(x_map);
        check_map_values!(y_map);

        Ok(Self {
            x_max_value: raw.x_max_value.unwrap_or(0),
            y_max_value: raw.y_max_value.unwrap_or(0),
            x_map: raw.x_map,
            y_map: raw.y_map,
            keymaps,
        })
    }
}

type ConfigChangeCallback = Box<dyn FnMut(Arc<Config>) + Send + Sync>;

pub struct WatchConfigChangeTask {
    path: PathBuf,
    filename: String,
    epoll: Epoll,
    inotify: Inotify,
    callbacks: Vec<ConfigChangeCallback>,
}
impl WatchConfigChangeTask {
    const EPOLL_CANCEL_EVENT: u64 = 0;
    const EPOLL_INOTIFY_EVENT: u64 = 1;
    const WATCH_CONFIG_CHANGE_DEBOUNCE: Duration = Duration::from_millis(500);

    pub fn new<P: AsRef<Path>>(path: P, cancel_token: CancelToken) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let filename = path
            .file_name()
            .context("无法从配置文件路径中提取文件名")?
            .to_str()
            .context("配置文件名称中包含无效字符")?
            .to_string();
        let mut parent_dir = path
            .parent()
            .context("无法从配置文件路径中提取父目录")?
            .to_path_buf();
        if parent_dir.as_os_str().is_empty() {
            parent_dir = PathBuf::from(".");
        }

        let cancel_eventfd =
            EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_SEMAPHORE)
                .context("EventFd::from_value_and_flags")?;
        let cancel_eventfd = Arc::new(cancel_eventfd);
        {
            let cancel_eventfd = cancel_eventfd.clone();
            cancel_token.register_callback(move || {
                if let Err(e) = cancel_eventfd.write(1) {
                    error!("无法通过写eventfd通知配置文件监视任务结束执行: {}", e);
                }
            });
        }

        let inotify = Inotify::init(inotify::InitFlags::all()).context("Inotify::init")?;
        inotify
            .add_watch(
                &parent_dir,
                inotify::AddWatchFlags::IN_MODIFY
                    | inotify::AddWatchFlags::IN_CREATE
                    | inotify::AddWatchFlags::IN_MOVED_TO,
            )
            .context("Inotify::add_watch")?;

        let epoll = Epoll::new(EpollCreateFlags::all()).context("Epoll::new")?;
        epoll
            .add(
                &cancel_eventfd,
                EpollEvent::new(EpollFlags::EPOLLIN, Self::EPOLL_CANCEL_EVENT),
            )
            .context("Epoll::add(EventFd)")?;
        epoll
            .add(
                &inotify,
                EpollEvent::new(EpollFlags::EPOLLIN, Self::EPOLL_INOTIFY_EVENT),
            )
            .context("Epoll::add(Inotify)")?;
        Ok(Self {
            path,
            filename,
            epoll,
            inotify,
            callbacks: Vec::new(),
        })
    }

    pub fn register_callback<F>(&mut self, f: F)
    where
        F: FnMut(Arc<Config>) + Send + Sync + 'static,
    {
        self.callbacks.push(Box::new(f));
    }

    pub fn run(mut self) -> Result<()> {
        let mut events = [EpollEvent::empty(); 1];
        loop {
            let n = self
                .epoll
                .wait(&mut events, EpollTimeout::NONE)
                .context("Epoll::wait")?;
            if n == 0 {
                continue;
            }
            match events[0].data() {
                x if x == Self::EPOLL_CANCEL_EVENT => return Ok(()),
                x if x == Self::EPOLL_INOTIFY_EVENT => {
                    let events = self.drain_inotify_events()?;
                    let mut modified = false;
                    for event in events {
                        if event.name.unwrap_or_default() == self.filename.as_str() {
                            modified = true;
                        }
                    }
                    if !modified {
                        continue;
                    }
                    std::thread::sleep(Self::WATCH_CONFIG_CHANGE_DEBOUNCE);
                    let _ = self.drain_inotify_events()?;
                    match Config::load(&self.path) {
                        Ok(conf) => {
                            let conf = Arc::new(conf);
                            for callback in &mut self.callbacks {
                                callback(conf.clone());
                            }
                        }
                        Err(e) => {
                            warn!("无法重新加载配置文件，忽略本次配置文件的变动: {e}");
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    fn drain_inotify_events(&self) -> Result<Vec<InotifyEvent>> {
        let mut result = vec![];
        loop {
            match self.inotify.read_events() {
                Ok(events) => result.extend(events),
                Err(Errno::EAGAIN) => return Ok(result),
                Err(e) => Err(e).context("Inotify::read_events")?,
            }
        }
    }
}
