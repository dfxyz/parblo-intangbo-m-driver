use std::thread::spawn;

use anyhow::{Context, Result};

use crate::{
    cancel::CancelToken,
    config::{Config, WatchConfigChangeTask},
    driver::DriverTask,
    signal::ExitSignal,
};

mod cancel;
mod config;
mod driver;
mod macros;
mod signal;

fn main() -> Result<()> {
    let ct = CancelToken::new();

    let conf_path = std::env::args().nth(1);
    let conf = match &conf_path {
        Some(path) => Config::load(path).context("加载配置文件失败")?,
        None => Config::default(),
    };

    let exit_signal = ExitSignal::new(ct.clone())?;

    let mut watch_config_change_task = None;
    if let Some(conf_path) = conf_path {
        watch_config_change_task.replace(
            WatchConfigChangeTask::new(conf_path, ct.clone())
                .context("初始化配置文件监控任务时发生错误")?,
        );
    }
    let driver_task = DriverTask::new(ct.clone(), conf, watch_config_change_task.as_mut())
        .context("初始化驱动任务时发生错误")?;

    let mut tasks = Vec::with_capacity(2);
    tasks.push(spawn(move || {
        if let Err(e) = exit_signal.wait() {
            error!("退出信号监控任务发生错误并退出: {:?}", e);
        }
    }));
    if let Some(task) = watch_config_change_task {
        tasks.push(spawn(move || {
            if let Err(e) = task.run() {
                error!("配置文件监控任务发生错误并退出: {:?}", e);
            }
        }));
    }

    if let Err(e) = driver_task.run() {
        error!("驱动任务发生错误并退出: {:?}", e);
    }
    ct.cancel();
    for task in tasks {
        if let Err(e) = task.join() {
            error!("任务意外退出: {:?}", e);
        }
    }
    Ok(())
}
