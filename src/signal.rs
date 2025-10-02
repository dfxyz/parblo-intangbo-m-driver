use std::sync::Arc;

use anyhow::{Context, Result};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::signal::{SigSet, SigmaskHow, Signal, sigprocmask};
use nix::sys::signalfd::{SfdFlags, SignalFd};

use crate::cancel::CancelToken;
use crate::{error, info};

pub struct ExitSignal {
    cancel_token: CancelToken,
    signalfd: SignalFd,
    epoll: Epoll,
}
impl ExitSignal {
    const EPOLL_CANCEL_EVENT: u64 = 0;
    const EPOLL_SIGNAL_EVENT: u64 = 1;

    pub fn new(cancel_token: CancelToken) -> Result<Self> {
        let mut sigset = SigSet::empty();
        sigset.add(Signal::SIGINT);
        sigset.add(Signal::SIGTERM);
        sigset.add(Signal::SIGHUP);
        sigprocmask(SigmaskHow::SIG_BLOCK, Some(&sigset), None).context("sigprocmask")?;
        let signalfd =
            SignalFd::with_flags(&sigset, SfdFlags::SFD_NONBLOCK).context("SignalFd::new")?;

        let cancel_eventfd =
            EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_SEMAPHORE)
                .context("EventFd::from_value_and_flags")?;
        let cancel_eventfd = Arc::new(cancel_eventfd);
        {
            let cancel_eventfd = cancel_eventfd.clone();
            cancel_token.register_callback(move || {
                if let Err(e) = cancel_eventfd.write(1) {
                    error!("无法通过写eventfd通知退出信号监视任务结束执行: {}", e);
                }
            });
        }

        let epoll = Epoll::new(EpollCreateFlags::all()).context("Epoll::new")?;
        epoll
            .add(
                &cancel_eventfd,
                EpollEvent::new(EpollFlags::EPOLLIN, Self::EPOLL_CANCEL_EVENT),
            )
            .context("Epoll::add(EventFd)")?;
        epoll
            .add(
                &signalfd,
                EpollEvent::new(EpollFlags::EPOLLIN, Self::EPOLL_SIGNAL_EVENT),
            )
            .context("Epoll::add(SignalFd)")?;

        Ok(Self { cancel_token, signalfd, epoll })
    }

    pub fn wait(self) -> Result<()> {
        let mut events = [EpollEvent::empty(); 1];
        loop {
            let n = self.epoll.wait(&mut events, EpollTimeout::NONE)?;
            if n == 0 {
                continue;
            }
            match events[0].data() {
                Self::EPOLL_CANCEL_EVENT => return Ok(()),
                Self::EPOLL_SIGNAL_EVENT => {
                    let siginfo = loop {
                        match self
                            .signalfd
                            .read_signal()
                            .context("SignalFd::read_signal")?
                        {
                            Some(x) => break x,
                            None => continue,
                        }
                    };
                    match siginfo.ssi_signo {
                        x if x == Signal::SIGINT as _ => {
                            info!("接收到SIGINT信号，准备退出");
                        }
                        x if x == Signal::SIGTERM as _ => {
                            info!("接收到SIGTERM信号，准备退出");
                        }
                        x if x == Signal::SIGHUP as _ => {
                            info!("接收到SIGHUP信号，准备退出");
                        }
                        _ => unreachable!(),
                    }
                    self.cancel_token.cancel();
                    return Ok(());
                }
                _ => unreachable!(),
            }
        }
    }
}
