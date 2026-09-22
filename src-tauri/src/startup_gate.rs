//! 启动闸门：setup 完成前到达的前端 IPC 暂存,setup 末尾按到达顺序重放。
//!
//! 原因：Tauri 在 `RunEvent:Ready` 里先按配置逐个创建窗口、**再**调用 setup 闭包;
//! WebView2 建窗期间会泵消息,先建好的窗口页面已加载并发出 IPC——`get_visibility` /
//! `window_ready` 会早于 setup 首行被执行,读到 AppState 初值（悬浮球不可见、未贴边、竖条）,
//! 而不是 `window_state:restore` 装载的落盘状态。窗口越多,前面窗口抢跑的时间越长。
//!
//! 做法：包一层 invoke handler——闸门未开时把 `Invoke` 原样入队（前端 promise 挂起等待,
//! 无需任何前端改动）;setup 末尾 `open` 按序重放。插件命令（事件订阅等）不经此处,
//! 不受影响;ACL 校验在 invoke handler 之前完成,重放不绕过权限。

use std::sync::{Arc, Mutex, OnceLock};

use tauri::ipc::Invoke;
use tauri::Wry;

type Handler = Box<dyn Fn(Invoke<Wry>) -> bool + Send + Sync>;

pub struct StartupGate {
    handler: OnceLock<Handler>,
    /// Some = 闸门未开（暂存队列）;None = 已开,直接分发。
    pending: Mutex<Option<Vec<Invoke<Wry>>>>,
}

impl StartupGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { handler: OnceLock::new(), pending: Mutex::new(Some(Vec::new())) })
    }

    /// 包装 `generate_handler!` 的结果,交给 `Builder:invoke_handler`。
    pub fn wrap(
        self: &Arc<Self>,
        handler: impl Fn(Invoke<Wry>) -> bool + Send + Sync + 'static,
    ) -> impl Fn(Invoke<Wry>) -> bool + Send + Sync + 'static {
        let _ = self.handler.set(Box::new(handler));
        let gate = Arc::clone(self);
        move |invoke| gate.dispatch(invoke)
    }

    fn handle(&self, invoke: Invoke<Wry>) -> bool {
        self.handler.get().is_some_and(|h| h(invoke))
    }

    fn dispatch(&self, invoke: Invoke<Wry>) -> bool {
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(queue) = pending.as_mut() {
                queue.push(invoke);
                return true;
            }
        }
        self.handle(invoke)
    }

    /// setup 末尾调用：开闸并按到达顺序重放暂存的调用（幂等）。
    pub fn open(&self) {
        let queued = self.pending.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(queued) = queued else { return };
        crate::dev_log!("[startup-gate] open, replaying {} early invoke(s)", queued.len());
        for invoke in queued {
            let cmd = invoke.message.command().to_string();
            if !self.handle(invoke) {
                // 未注册命令（前端只调注册过的命令,理论不可达）：resolver 已随 invoke 移入
                // handler,无法再 reject——记日志供诊断
                crate::dev_log!("[startup-gate] replayed invoke not handled: {cmd}");
            }
        }
    }
}
