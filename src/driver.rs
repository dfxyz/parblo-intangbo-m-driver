use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use evdev_rs::enums::{EV_ABS, EV_KEY, EV_SYN, EventCode, EventType, InputProp};
use evdev_rs::{
    AbsInfo, Device as EventDevice, DeviceWrapper, InputEvent, TimeVal, UInputDevice, UninitDevice,
};
use parking_lot::Mutex;
use rusb::{DeviceHandle as UsbDeviceHandle, Error as UsbError, UsbContext};

use crate::cancel::CancelToken;
use crate::config::{Config, Keymap, WatchConfigChangeTask};
use crate::{debug, info, warn};

const VENDOR_ID: u16 = 0x0483;
const PRODUCT_ID: u16 = 0xa013;
const INTERFACE_NUM: u8 = 0x02;
const IN_ENDPOINT: u8 = 0x83;
const OUT_ENDPOINT: u8 = 0x03;
const HANDSHAKE_USAGE_BUF_SIZE: usize = 1101;
const INPUT_USAGE_BUF_SIZE: usize = 10;
const READ_INTERRUPT_TIMEOUT: Duration = Duration::from_millis(1000);
const EVENT_DEVICE_NAME: &str = "  Parblo Intangbo  M(F7)";
const VIRTUAL_DIGITIZER_NAME: &str = "Parblo Intangbo M (Digitizer)";
const VIRTUAL_KEYBOARD_NAME: &str = "Parblo Intangbo M (Keyboard)";
const DEVICE_HANDSHAKE_DATA_LIST: &[&[u8]] = &[
    &[
        0xfd, 0x89, 0xff, 0xff, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x03, 0x01, 0x01, 0x01, 0x91,
        0x20,
    ],
    &[
        0xfd, 0x89, 0xff, 0xff, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x01, 0x01, 0x02, 0x02, 0xfd,
        0x58,
    ],
    &[
        0xfd, 0x89, 0xff, 0xff, 0x00, 0x02, 0x00, 0x06, 0x00, 0x00, 0x01, 0x01, 0x02, 0x04, 0x4e,
        0x69,
    ],
    &[0x02, 0xb0, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00],
];

pub struct DriverTask {
    cancel_token: CancelToken,
    digitizer_uinput: UInputDevice,
    keyboard_uinput: UInputDevice,
    handle: UsbDeviceHandle<rusb::GlobalContext>,
    conf: Config,
    latest_conf: Arc<Mutex<Option<Config>>>,
    keymap_index: usize,
    pressed_keys: HashSet<EV_KEY>, // 设备本身不支持同时按下多个键，因此可直接用集合记录某个键的按键码组合
    stylus: StylusStatus,
}
struct StylusStatus {
    in_area: bool,
    tip_pressed: bool,
    pressure: u16,
    button0_pressed: bool,
    button1_pressed: bool,
    x: u16,
    y: u16,
    tilt_x: i8,
    tilt_y: i8,
}
impl DriverTask {
    pub fn new(
        cancel_token: CancelToken,
        mut conf: Config,
        watch_config_change_task: Option<&mut WatchConfigChangeTask>,
    ) -> Result<Self> {
        let (digitizer_uinput, keyboard_uinput) =
            create_uinput_device(&mut conf).context("无法创建虚拟设备")?;
        let handle = open_usb_device_handle().context("无法打开USB设备句柄")?;

        let latest_conf = Arc::new(Mutex::new(None));
        if let Some(task) = watch_config_change_task {
            let latest_conf = latest_conf.clone();
            task.register_callback(move |conf| {
                latest_conf.lock().replace(conf.as_ref().clone());
            });
        }

        Ok(Self {
            cancel_token,
            digitizer_uinput,
            keyboard_uinput,
            handle,
            conf,
            latest_conf,
            keymap_index: 0,
            pressed_keys: HashSet::new(),
            stylus: StylusStatus {
                in_area: false,
                tip_pressed: false,
                pressure: 0,
                button0_pressed: false,
                button1_pressed: false,
                x: 0,
                y: 0,
                tilt_x: 0,
                tilt_y: 0,
            },
        })
    }

    pub fn run(mut self) -> Result<()> {
        info!("驱动任务开始运行");
        loop {
            if self.cancel_token.cancelled() {
                return Ok(());
            }
            self.check_config_change();
            self.read_and_handle_device_input()?;
        }
    }

    fn check_config_change(&mut self) {
        let mut latest_conf = match self.latest_conf.lock().take() {
            Some(keymaps) => keymaps,
            None => return,
        };
        {
            // 修正不支持热更的字段
            latest_conf.x_max_value = self.conf.x_max_value;
            latest_conf.y_max_value = self.conf.y_max_value;
        }
        if latest_conf.keymaps.len() >= self.conf.keymaps.len() {
            info!(
                "已重新加载配置文件；继续使用按键映射方案{}",
                self.keymap_index
            );
        } else {
            info!("已重新加载配置文件；切换到按键映射方案0");
            self.keymap_index = 0;
        }
        self.conf = latest_conf;
    }

    fn read_and_handle_device_input(&mut self) -> Result<()> {
        let mut buf = [0u8; INPUT_USAGE_BUF_SIZE];
        loop {
            match self
                .handle
                .read_interrupt(IN_ENDPOINT, &mut buf, READ_INTERRUPT_TIMEOUT)
            {
                Ok(len) => {
                    self.handle_device_input(&buf[..len])?;
                }
                Err(UsbError::Timeout) => {
                    return Ok(());
                }
                Err(e) => {
                    return Err(anyhow!("读取USB设备的中断端点时发生错误: {}", e));
                }
            }
        }
    }

    fn handle_device_input(&mut self, buf: &[u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        if buf[0] != 0x02 {
            warn!("收到非0x02用途的中断输入：{:02x?}", buf);
            return Ok(());
        }
        let buf = &buf[1..];
        match buf[0] & 0xf0 {
            0xf0 => {
                self.handle_button_event(buf)?;
            }
            _ => {
                self.handle_digitizer_event(buf)?;
            }
        }
        Ok(())
    }

    fn handle_button_event(&mut self, buf: &[u8]) -> Result<()> {
        let code = ((buf[1] as u16) << 8) | (buf[2] as u16);
        macro_rules! handle {
            ($desc:literal, $field:ident) => {
                debug!($desc);
                match &self
                    .conf
                    .keymaps
                    .get(self.keymap_index)
                    .context("按键映射方案下标越界")?
                    .$field
                {
                    Keymap::Press(codes) => {
                        for code in codes.iter() {
                            debug!("虚拟键盘 - 按下{:?}", code);
                            self.write_keyboard_event(EventCode::EV_KEY(*code), 1)?;
                            self.pressed_keys.insert(*code);
                        }
                        self.write_keyboard_event(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0)?;
                    }
                    Keymap::SwitchSchema => {
                        self.switch_schema();
                    }
                    _ => {}
                }
            };
        }
        match code {
            0x0000 => {
                debug!("收到释放按键事件");
                if !self.pressed_keys.is_empty() {
                    for code in self.pressed_keys.iter() {
                        debug!("虚拟键盘 - 释放{:?}", code);
                        self.write_keyboard_event(EventCode::EV_KEY(*code), 0)?;
                    }
                    self.pressed_keys.clear();
                    self.write_keyboard_event(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0)?;
                }
            }
            0x0100 => {
                handle!("收到按下按钮0事件", button0);
            }
            0x0200 => {
                handle!("收到按下按钮1事件", button1);
            }
            0x0400 => {
                handle!("收到按下按钮2事件", button2);
            }
            0x0800 => {
                handle!("收到按下按钮3事件", button3);
            }
            0x0801 => {
                handle!("收到顺时针转动转环事件", ring1);
            }
            0x0802 => {
                handle!("收到逆时针转动转环事件", ring0);
            }
            0x0803 => {
                handle!("收到按下转环中心按钮事件", ring_button);
            }
            0x1000 => {
                handle!("收到按下按钮4事件", button4);
            }
            0x2000 => {
                handle!("收到按下按钮5事件", button5);
            }
            0x4000 => {
                handle!("收到按下按钮6事件", button6);
            }
            0x8000 => {
                handle!("收到按下按钮7事件", button7);
            }
            _ => {
                warn!("收到了未知的按键事件：{:02x?}", buf);
            }
        }
        Ok(())
    }

    fn switch_schema(&mut self) {
        let len = self.conf.keymaps.len();
        let current_index = self.keymap_index;
        let new_index = (current_index + 1) % len;
        if new_index != current_index {
            self.keymap_index = new_index;
            info!("已切换到按键映射方案{}", new_index);
        }
    }

    fn write_keyboard_event(&self, code: EventCode, value: i32) -> Result<()> {
        let dummy_timeval = TimeVal::new(0, 0);
        self.keyboard_uinput
            .write_event(&InputEvent::new(&dummy_timeval, &code, value))
            .context("UInputDevice::write_event(keyboard)")
    }

    fn handle_digitizer_event(&mut self, buf: &[u8]) -> Result<()> {
        let stylus_in_area = match buf[0] & 0xf0 {
            0xa0 => true,
            0xc0 => false,
            _ => {
                warn!("收到了未知的绘图板事件：{:02x?}", buf);
                return Ok(());
            }
        };
        let stylus_touching = buf[0] & (0x01 << 0) != 0;
        let stylus0_pressed = buf[0] & (0x01 << 1) != 0;
        let stylus1_pressed = buf[0] & (0x01 << 2) != 0;
        let y = u16::from_le_bytes([buf[1], buf[2]]); // 调换原始输入的X、Y坐标
        let x = u16::from_le_bytes([buf[3], buf[4]]);
        let pressure = u16::from_le_bytes([buf[5], buf[6]]);
        let x_tilt = i8::from_le_bytes([buf[7]]);
        let y_tilt = i8::from_le_bytes([buf[8]]);
        debug!(
            "收到绘图板事件：感应区域({})，笔尖({})，笔侧键({},{})，坐标({},{})，压力({})，倾斜({},{})",
            stylus_in_area,
            stylus_touching,
            stylus0_pressed,
            stylus1_pressed,
            x,
            y,
            pressure,
            x_tilt,
            y_tilt
        );

        // 进入/离开感应区域
        {
            if stylus_in_area {
                if !self.stylus.in_area {
                    debug!("虚拟绘图板 - 笔尖进入感应区域");
                    self.stylus.in_area = true;
                    self.write_digitizer_event(EventCode::EV_KEY(EV_KEY::BTN_TOOL_PEN), 1)?;
                    {
                        self.write_digitizer_x(x, true)?;
                        self.write_digitizer_y(y, true)?;
                    }
                    self.write_digitizer_event(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0)?;
                    return Ok(());
                }
            } else {
                if self.stylus.in_area {
                    {
                        self.write_digitizer_tip_released()?;
                        self.write_digitizer_x(x, false)?;
                        self.write_digitizer_y(y, false)?;
                        self.write_digitizer_tilt_x(0)?;
                        self.write_digitizer_tilt_y(0)?;
                        self.write_digitizer_button0_released()?;
                        self.write_digitizer_button1_released()?;
                    }
                    debug!("虚拟绘图板 - 笔尖离开感应区域");
                    self.write_digitizer_event(EventCode::EV_KEY(EV_KEY::BTN_TOOL_PEN), 0)?;
                    self.stylus.in_area = false;
                    self.write_digitizer_event(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0)?;
                    return Ok(());
                }
            }
        }

        // 笔尖按下/释放
        {
            if stylus_touching {
                if self.write_digitizer_tip_pressed()? {
                    self.write_digitizer_x(x, false)?;
                    self.write_digitizer_y(y, false)?;
                    self.write_digitizer_event(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0)?;
                    return Ok(());
                }
            } else {
                if self.write_digitizer_tip_released()? {
                    self.write_digitizer_x(x, false)?;
                    self.write_digitizer_y(y, false)?;
                    self.write_digitizer_event(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0)?;
                    return Ok(());
                }
            }
        }

        // 其他事件
        let mut report = false;
        if stylus0_pressed {
            report |= self.write_digitizer_button0_pressed()?;
        } else {
            report |= self.write_digitizer_button0_released()?;
        }
        if stylus1_pressed {
            report |= self.write_digitizer_button1_pressed()?;
        } else {
            report |= self.write_digitizer_button1_released()?;
        }
        report |= self.write_digitizer_tip_pressure(pressure, false)?;
        report |= self.write_digitizer_x(x, false)?;
        report |= self.write_digitizer_y(y, false)?;
        report |= self.write_digitizer_tilt_x(x_tilt)?;
        report |= self.write_digitizer_tilt_y(y_tilt)?;
        if report {
            self.write_digitizer_event(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0)?;
        }
        Ok(())
    }

    fn write_digitizer_event(&self, code: EventCode, value: i32) -> Result<()> {
        let dummy_timeval = TimeVal::new(0, 0);
        self.digitizer_uinput
            .write_event(&InputEvent::new(&dummy_timeval, &code, value))
            .context("UInputDevice::write_event(digitizer)")
    }

    fn write_digitizer_x(&mut self, x: u16, force: bool) -> Result<bool> {
        let x = std::cmp::min(x, self.conf.x_max_value);
        let x = match self.conf.x_map {
            Some((min_ratio, max_ratio)) => (self.conf.x_max_value as f32 * min_ratio
                + (x as f32 * (max_ratio - min_ratio)))
                .round() as u16,
            None => x,
        };
        if !force && x == self.stylus.x {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 上报X坐标({})", x);
        self.stylus.x = x;
        self.write_digitizer_event(EventCode::EV_ABS(EV_ABS::ABS_X), x as i32)?;
        Ok(true)
    }

    fn write_digitizer_y(&mut self, y: u16, force: bool) -> Result<bool> {
        let y = std::cmp::min(y, self.conf.y_max_value);
        let y = match self.conf.y_map {
            Some((min_ratio, max_ratio)) => (self.conf.y_max_value as f32 * min_ratio
                + (y as f32 * (max_ratio - min_ratio)))
                .round() as u16,
            None => y,
        };
        let y = self.conf.y_max_value - y; // 需要再翻转一次
        if !force && y == self.stylus.y {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 上报Y坐标({})", y);
        self.stylus.y = y;
        self.write_digitizer_event(EventCode::EV_ABS(EV_ABS::ABS_Y), y as i32)?;
        Ok(true)
    }

    fn write_digitizer_tip_pressed(&mut self) -> Result<bool> {
        if self.stylus.tip_pressed {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 笔尖按下");
        self.stylus.tip_pressed = true;
        self.write_digitizer_event(EventCode::EV_KEY(EV_KEY::BTN_TOUCH), 1)?;
        self.write_digitizer_tip_pressure(1, true)?;
        Ok(true)
    }

    fn write_digitizer_tip_released(&mut self) -> Result<bool> {
        if !self.stylus.tip_pressed {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 笔尖释放");
        self.stylus.tip_pressed = false;
        self.write_digitizer_event(EventCode::EV_KEY(EV_KEY::BTN_TOUCH), 0)?;
        self.write_digitizer_tip_pressure(0, true)?;
        Ok(true)
    }

    fn write_digitizer_tip_pressure(&mut self, pressure: u16, force: bool) -> Result<bool> {
        if !force && pressure == self.stylus.pressure {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 上报笔尖压力({})", pressure);
        self.stylus.pressure = pressure;
        self.write_digitizer_event(EventCode::EV_ABS(EV_ABS::ABS_PRESSURE), pressure as i32)?;
        Ok(true)
    }

    fn write_digitizer_button0_pressed(&mut self) -> Result<bool> {
        if self.stylus.button0_pressed {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 按下下方的笔侧键");
        self.stylus.button0_pressed = true;
        self.write_digitizer_event(EventCode::EV_KEY(EV_KEY::BTN_STYLUS), 1)?;
        Ok(true)
    }

    fn write_digitizer_button0_released(&mut self) -> Result<bool> {
        if !self.stylus.button0_pressed {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 释放下方的笔侧键");
        self.stylus.button0_pressed = false;
        self.write_digitizer_event(EventCode::EV_KEY(EV_KEY::BTN_STYLUS), 0)?;
        Ok(true)
    }

    fn write_digitizer_button1_pressed(&mut self) -> Result<bool> {
        if self.stylus.button1_pressed {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 按下上方的笔侧键");
        self.stylus.button1_pressed = true;
        self.write_digitizer_event(EventCode::EV_KEY(EV_KEY::BTN_STYLUS2), 1)?;
        Ok(true)
    }

    fn write_digitizer_button1_released(&mut self) -> Result<bool> {
        if !self.stylus.button1_pressed {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 释放上方的笔侧键");
        self.stylus.button1_pressed = false;
        self.write_digitizer_event(EventCode::EV_KEY(EV_KEY::BTN_STYLUS2), 0)?;
        Ok(true)
    }

    fn write_digitizer_tilt_x(&mut self, tilt_x: i8) -> Result<bool> {
        if tilt_x == self.stylus.tilt_x {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 上报笔尖倾斜({})", tilt_x);
        self.stylus.tilt_x = tilt_x;
        self.write_digitizer_event(EventCode::EV_ABS(EV_ABS::ABS_TILT_X), tilt_x as i32)?;
        Ok(true)
    }

    fn write_digitizer_tilt_y(&mut self, tilt_y: i8) -> Result<bool> {
        if tilt_y == self.stylus.tilt_y {
            return Ok(false);
        }
        debug!("虚拟绘图板 - 上报笔尖倾斜({})", tilt_y);
        self.stylus.tilt_y = tilt_y;
        self.write_digitizer_event(EventCode::EV_ABS(EV_ABS::ABS_TILT_Y), tilt_y as i32)?;
        Ok(true)
    }
}

fn create_uinput_device(conf: &mut Config) -> Result<(UInputDevice, UInputDevice)> {
    let evdev = open_evdev().context("open_evdev")?;
    let digitizer = create_uninit_digitizer_from_evdev(conf, &evdev)
        .context("create_uninit_digitizer_from_evdev")?;
    let keyboard =
        create_uninit_keyboard_from_evdev(&evdev).context("create_uninit_keyboard_from_evdev")?;
    let digitizer_uinput =
        UInputDevice::create_from_device(&digitizer).context("UInputDevice::create_from_device")?;
    let keyboard_uinput =
        UInputDevice::create_from_device(&keyboard).context("UInputDevice::create_from_device")?;
    Ok((digitizer_uinput, keyboard_uinput))
}

fn open_evdev() -> Result<EventDevice> {
    let entries = std::fs::read_dir("/dev/input").context("无法读取目录/dev/input")?;
    for entry in entries {
        let entry = entry.context("无法读取目录/dev/input中的信息")?;
        let filename = entry.file_name();
        if !filename.to_string_lossy().starts_with("event") {
            continue;
        }
        let path = entry.path();
        let device = EventDevice::new_from_path(&path).context("EventDevice::new_from_path")?;
        if device.name().unwrap_or_default() == EVENT_DEVICE_NAME {
            return Ok(device);
        }
    }
    Err(anyhow!("找不到「Parblo Intangbo M」对应的EventDevice"))
}

macro_rules! enable_key_code {
    ($ud:ident => $($code:ident),+ $(,)?) => {
        $(
            $ud.enable_event_code(&EventCode::EV_KEY(EV_KEY::$code), None).context(concat!("UninitDevice::enable_event_code(EV_KEY::", stringify!($code), ")"))?;
        )+
    };
}

fn create_uninit_digitizer_from_evdev(
    conf: &mut Config,
    evdev: &EventDevice,
) -> Result<UninitDevice> {
    let ud = UninitDevice::new().context("UninitDevice::new")?;
    ud.set_name(VIRTUAL_DIGITIZER_NAME);
    ud.set_bustype(evdev.bustype());
    ud.set_vendor_id(evdev.vendor_id());
    ud.set_product_id(evdev.product_id());
    ud.set_version(evdev.version());

    macro_rules! read_abs_info {
        ($name:ident) => {{
            let mut info = evdev
                .abs_info(&EventCode::EV_ABS(EV_ABS::$name))
                .context(concat!("EventDevice::abs_info(", stringify!($name), ")"))?;
            info.value = 0;
            info
        }};
    }
    let mut abs_x = read_abs_info!(ABS_X);
    if conf.y_max_value > 0 {
        abs_x.maximum = conf.y_max_value as i32; // ABS_X与ABS_Y需要互相调换
    } else {
        conf.y_max_value = abs_x.maximum as u16;
    }
    let mut abs_y = read_abs_info!(ABS_Y);
    if conf.x_max_value > 0 {
        abs_y.maximum = conf.x_max_value as i32; // ABS_X与ABS_Y需要互相调换
    } else {
        conf.x_max_value = abs_y.maximum as u16;
    }
    let abs_pressure = read_abs_info!(ABS_PRESSURE);
    let abs_tilt_x = read_abs_info!(ABS_TILT_X);
    let abs_tilt_y = read_abs_info!(ABS_TILT_Y);

    macro_rules! copy_abs_info {
        ($dst:ident, $src:expr) => {
            ud.enable_event_code(
                &EventCode::EV_ABS(EV_ABS::$dst),
                Some(evdev_rs::EnableCodeData::AbsInfo(AbsInfo {
                    minimum: $src.minimum,
                    maximum: $src.maximum,
                    resolution: $src.resolution,
                    value: 0,
                    fuzz: 0,
                    flat: 0,
                })),
            )
            .context(concat!(
                "UninitDevice::enable_event_code(",
                stringify!($dst),
                ")"
            ))?;
        };
    }
    copy_abs_info!(ABS_X, &abs_y); // ABS_X与ABS_Y需要互相调换
    copy_abs_info!(ABS_Y, &abs_x); // ABS_X与ABS_Y需要互相调换
    copy_abs_info!(ABS_PRESSURE, &abs_pressure);
    copy_abs_info!(ABS_TILT_X, &abs_tilt_x);
    copy_abs_info!(ABS_TILT_Y, &abs_tilt_y);

    ud.enable_event_type(&EventType::EV_SYN)
        .context("UninitDevice::enable_event_type(EV_SYN)")?;
    ud.enable_property(&InputProp::INPUT_PROP_POINTER)
        .context("UninitDevice::enable_property(INPUT_PROP_POINTER)")?;
    enable_key_code! { ud => BTN_TOOL_PEN, BTN_TOOL_RUBBER, BTN_TOUCH, BTN_STYLUS, BTN_STYLUS2 };
    Ok(ud)
}

fn create_uninit_keyboard_from_evdev(evdev: &EventDevice) -> Result<UninitDevice> {
    let ud = UninitDevice::new().context("UninitDevice::new")?;
    ud.set_name(VIRTUAL_KEYBOARD_NAME);
    ud.set_bustype(evdev.bustype());
    ud.set_vendor_id(evdev.vendor_id());
    ud.set_product_id(evdev.product_id());
    ud.set_version(evdev.version());

    ud.enable_event_type(&EventType::EV_SYN)
        .context("UninitDevice::enable_event_type(EV_SYN)")?;
    ud.enable_event_type(&EventType::EV_REP)
        .context("UninitDevice::enable_event_type(EV_REP)")?;
    enable_key_code! { ud =>
        KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F, KEY_G, KEY_H, KEY_I, KEY_J, KEY_K, KEY_L, KEY_M,
        KEY_N, KEY_O, KEY_P, KEY_Q, KEY_R, KEY_S, KEY_T, KEY_U, KEY_V, KEY_W, KEY_X, KEY_Y, KEY_Z,

        KEY_0, KEY_1, KEY_2, KEY_3, KEY_4, KEY_5, KEY_6, KEY_7, KEY_8, KEY_9,

        KEY_MINUS, KEY_EQUAL, KEY_BACKSLASH, KEY_GRAVE, KEY_LEFTBRACE, KEY_RIGHTBRACE,
        KEY_SEMICOLON, KEY_APOSTROPHE, KEY_COMMA, KEY_DOT, KEY_SLASH,

        KEY_ESC, KEY_TAB, KEY_BACKSPACE, KEY_SPACE, KEY_ENTER,
        KEY_HOME, KEY_END, KEY_PAGEUP, KEY_PAGEDOWN, KEY_INSERT, KEY_DELETE,

        KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_LEFTALT, KEY_LEFTMETA,
    };
    Ok(ud)
}

fn open_usb_device_handle() -> Result<UsbDeviceHandle<rusb::GlobalContext>> {
    let ctx = rusb::GlobalContext {};
    let handle = ctx
        .open_device_with_vid_pid(VENDOR_ID, PRODUCT_ID)
        .context("UsbDeviceHandle::open_device_with_vid_pid")?;

    if handle
        .kernel_driver_active(INTERFACE_NUM)
        .context("UsbDeviceHandle::kernel_driver_active")?
    {
        handle
            .detach_kernel_driver(INTERFACE_NUM)
            .context("UsbDeviceHandle::detach_kernel_driver")?;
    }
    handle
        .claim_interface(INTERFACE_NUM)
        .context("UsbDeviceHandle::claim_interface")?;

    for (i, msg) in DEVICE_HANDSHAKE_DATA_LIST.iter().enumerate() {
        let mut concat_msg = None;
        let buf = if msg[0] == 0xfd {
            let padding_len = HANDSHAKE_USAGE_BUF_SIZE - msg.len();
            concat_msg.replace(
                msg.iter()
                    .cloned()
                    .chain(std::iter::repeat_n(0u8, padding_len))
                    .collect::<Vec<_>>(),
            );
            concat_msg.as_ref().unwrap()
        } else {
            *msg
        };
        handle
            .write_interrupt(OUT_ENDPOINT, buf, READ_INTERRUPT_TIMEOUT)
            .context(format!("UsbDeviceHandle::write_interrupt({})", i))?;
        let mut buf = [0u8; HANDSHAKE_USAGE_BUF_SIZE];
        handle
            .read_interrupt(IN_ENDPOINT, &mut buf, READ_INTERRUPT_TIMEOUT)
            .context(format!("UsbDeviceHandle::read_interrupt({})", i))?;
    }
    Ok(handle)
}
