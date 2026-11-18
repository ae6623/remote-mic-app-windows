//! Windows UI Automation 后端（仅 Windows）。
//!
//! 约束：UIA 客户端必须在**不拥有窗口的 MTA 线程**上调用（MS Learn
//! `uiauto-threading`），因此本模块自己起一个专用工作线程，在其上
//! `CoInitializeEx(MULTITHREADED)` 并创建 `CUIAutomation`；调用方通过 channel
//! 提交任务并同步等待结果。工作线程不创建任何窗口。
//!
//! 实测依据：`docs/investigations/2026-10-01-windows-input-focus-uia-feasibility.md`
//! —— 候选扫描 20–200 ms、`BoundingRectangle` 可能是离屏或饱和值（最小化窗口）、
//! Chromium 的 contenteditable 会以 `Group` 暴露、WorkBuddy 这类应用完全没有树。

#[cfg(windows)]
use crate::focus::{
    best_composer_index, best_recorded_index, FocusCandidate, FocusChoice, RecordedFocusTarget,
};
#[cfg(windows)]
use crate::focus_service::{AttemptResult, FocusFailure};

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use windows::core::{Interface, BSTR, PCWSTR};
    use windows::Win32::Foundation::{
        HWND, LPARAM, POINT, RECT, VARIANT_FALSE, VARIANT_TRUE, WPARAM,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Ole::SafeArrayDestroy;
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_I4};
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationCondition,
        IUIAutomationElement, IUIAutomationElementArray, IUIAutomationValuePattern,
        TreeScope_Children, TreeScope_Descendants, UIA_AutomationIdPropertyId,
        UIA_BoundingRectanglePropertyId, UIA_ClassNamePropertyId, UIA_ControlTypePropertyId,
        UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_GroupControlTypeId,
        UIA_HasKeyboardFocusPropertyId, UIA_IsEnabledPropertyId, UIA_IsKeyboardFocusablePropertyId,
        UIA_IsPasswordPropertyId, UIA_IsTextPatternAvailablePropertyId, UIA_NamePropertyId,
        UIA_TextPatternId, UIA_ValuePatternId, UIA_CONTROLTYPE_ID, UIA_PATTERN_ID,
    };
    use windows::Win32::UI::HiDpi::{
        GetThreadDpiAwarenessContext, SetThreadDpiAwarenessContext,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, EnumWindows, GetClassNameW, GetForegroundWindow, GetPhysicalCursorPos,
        GetWindowThreadProcessId, IsIconic, IsWindowVisible, SendMessageTimeoutW, OBJID_CLIENT,
        SMTO_ABORTIFHUNG, WM_GETOBJECT,
    };

    /// 单次遍历的元素上限（防止异常应用把树撑爆）。
    const MAX_TRAVERSAL_ELEMENTS: usize = 4000;
    /// 坐标合法性上限：UIA 的 `Rect` 在 windows-rs 里映射为 i32，最小化窗口会被
    /// 饱和成 `i32::MIN`/`i32::MAX`，离屏窗口还会给出 ±32000 级别的值。
    const MAX_ABS_COORDINATE: i32 = 1_000_000;

    struct Session {
        automation: IUIAutomation,
    }

    type Job = Box<dyn FnOnce(&Session) + Send>;

    struct Worker {
        sender: Sender<Job>,
    }

    static WORKER: OnceLock<Mutex<Worker>> = OnceLock::new();

    /// 取（或惰性启动）UIA 工作线程。
    fn worker() -> &'static Mutex<Worker> {
        WORKER.get_or_init(|| {
            let (sender, receiver) = channel::<Job>();
            std::thread::Builder::new()
                .name("sayall-uia".to_owned())
                .spawn(move || {
                    // UIA 的坐标随线程 DPI 感知变化：固定在 Per-Monitor V2 下取物理
                    // 像素，元素矩形与窗口矩形才在同一坐标系（后续若要按坐标点击，
                    // 也必须用物理像素）。退出前成对恢复。
                    let previous_awareness = unsafe { GetThreadDpiAwarenessContext() };
                    let awareness_switched = !previous_awareness.0.is_null()
                        && !unsafe {
                            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
                        }
                        .0
                        .is_null();
                    // 工作线程不拥有窗口；COM 以 MTA 初始化，符合 UIA 客户端要求。
                    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                    if initialized.is_err() {
                        if awareness_switched {
                            unsafe { SetThreadDpiAwarenessContext(previous_awareness) };
                        }
                        return;
                    }
                    let automation: IUIAutomation = match unsafe {
                        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                    } {
                        Ok(automation) => automation,
                        Err(_) => {
                            unsafe { CoUninitialize() };
                            if awareness_switched {
                                unsafe { SetThreadDpiAwarenessContext(previous_awareness) };
                            }
                            return;
                        }
                    };
                    let session = Session { automation };
                    while let Ok(job) = receiver.recv() {
                        job(&session);
                    }
                    unsafe { CoUninitialize() };
                    if awareness_switched {
                        unsafe { SetThreadDpiAwarenessContext(previous_awareness) };
                    }
                })
                .expect("启动 UIA 工作线程失败");
            Mutex::new(Worker { sender })
        })
    }

    /// 在工作线程上执行任务并等待结果；工作线程不可用时返回 `None`。
    fn submit<T, F>(task: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce(&Session) -> T + Send + 'static,
    {
        let (reply_sender, reply_receiver) = channel::<T>();
        {
            let guard = worker().lock().ok()?;
            let job: Job = Box::new(move |session| {
                let _ = reply_sender.send(task(session));
            });
            if guard.sender.send(job).is_err() {
                return None;
            }
        }
        reply_receiver.recv().ok()
    }

    /// 在共享 UIA 工作线程上执行任务并返回结果（线程用 Per-Monitor V2、MTA 初始化，
    /// 元素矩形与窗口矩形同坐标系）。供托盘图标等"只读 UIA + 触发用户可见控件"的功能
    /// 复用，避免各写一份 COM/DPI 初始化。工作线程不可用时返回 `None`。
    pub(crate) fn with_automation<T, F>(task: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce(&IUIAutomation) -> T + Send + 'static,
    {
        submit(move |session| task(&session.automation))
    }

    /// 与 [`with_automation`] 相同，但最多等待 `timeout`。回眸捕获是后台尽力而为：
    /// 目标 UIA provider 卡住时必须尽快放弃，不能让语音线程或捕获线程无限等待。
    pub(crate) fn with_automation_timeout<T, F>(timeout: Duration, task: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce(&IUIAutomation) -> T + Send + 'static,
    {
        let (reply_sender, reply_receiver) = channel::<T>();
        {
            let guard = worker().lock().ok()?;
            let job: Job = Box::new(move |session| {
                let _ = reply_sender.send(task(&session.automation));
            });
            if guard.sender.send(job).is_err() {
                return None;
            }
        }
        reply_receiver.recv_timeout(timeout).ok()
    }

    // ---------- Win32（不需要 UIA，可在任意线程调用） ----------

    pub fn current_process_id() -> u32 {
        std::process::id()
    }

    pub fn foreground_process_id() -> Option<u32> {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        (pid != 0).then_some(pid)
    }

    /// 当前前台进程的完整可执行文件路径（读不到为 `None`）。
    pub fn foreground_executable_path() -> Option<String> {
        let pid = foreground_process_id()?;
        crate::app_launcher::process_image_path(pid)
    }

    pub fn process_alive(pid: u32) -> bool {
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return false;
        };
        let mut code = 0u32;
        let alive = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok() && code == 259;
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
        alive
    }

    /// 窗口类名（`GetClassNameW` 包装；失败为空串）。只用于选择后端与日志，
    /// 不读取标题或其他内容。
    pub(crate) fn window_class_name(hwnd: HWND) -> String {
        let mut buffer = [0u16; 256];
        let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
        if length <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..length as usize])
    }

    /// 目标进程当前可交互的顶层窗口（可见且未最小化时取**面积最大**的一个；
    /// 全部隐藏时退回最大的隐藏主窗口），供 UIA 扫描使用。返回
    /// `(hwnd, 是否有可见窗口)`。
    ///
    /// 不再「取 Z 序第一个可见窗口」：Chromium/Electron 会创建 1×1 的辅助
    /// 顶层窗口，Z 序靠前时会被误选，随后扫描/学习得到 `window=1x1`。
    fn target_window(pid: u32) -> Option<(HWND, bool)> {
        struct Context {
            pid: u32,
            visible: Option<(HWND, i64)>,
            hidden: Option<(HWND, i64)>,
        }
        unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
            let context = unsafe { &mut *(lparam.0 as *mut Context) };
            let mut window_pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut window_pid)) };
            if window_pid != context.pid {
                return true.into();
            }
            let mut rect = RECT::default();
            if unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect) }
                .is_err()
            {
                return true.into();
            }
            let area = i64::from(rect.right - rect.left) * i64::from(rect.bottom - rect.top);
            let slot = if unsafe { IsWindowVisible(hwnd) }.as_bool()
                && !unsafe { IsIconic(hwnd) }.as_bool()
            {
                &mut context.visible
            } else {
                &mut context.hidden
            };
            if slot.is_none_or(|(_, best)| area > best) {
                *slot = Some((hwnd, area));
            }
            true.into()
        }

        let mut context = Context {
            pid,
            visible: None,
            hidden: None,
        };
        unsafe {
            let _ = EnumWindows(
                Some(callback),
                LPARAM(&mut context as *mut Context as isize),
            );
        }
        match (context.visible, context.hidden) {
            (Some((hwnd, _)), _) => Some((hwnd, true)),
            (None, Some((hwnd, _))) => Some((hwnd, false)),
            (None, None) => None,
        }
    }

    /// 触发 Chromium/Electron 的按需可访问性。
    ///
    /// Chromium 只有在检测到 UIA 客户端（`WM_GETOBJECT`/UIA 调用）后才构建
    /// 完整可访问性树；在此之前 `FindAll` 只会返回 0–2 个占位节点。这里向
    /// 顶层窗口与渲染子窗口发送 `WM_GETOBJECT(OBJID_CLIENT)`，再做一次浅层
    /// 查询把客户端标记置位。调用方随后等待一小段时间让渲染进程建树。
    pub(crate) fn warm_accessibility(automation: &IUIAutomation, hwnd: HWND) {
        unsafe extern "system" fn warm_child(hwnd: HWND, _: LPARAM) -> windows::core::BOOL {
            let mut buffer = [0u16; 256];
            let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
            if length > 0 {
                let class = String::from_utf16_lossy(&buffer[..length as usize]);
                if class.contains("Chrome_RenderWidgetHost") || class.contains("Chrome_WidgetWin") {
                    unsafe { request_accessible(hwnd) };
                }
            }
            true.into()
        }

        unsafe {
            request_accessible(hwnd);
            let _ = EnumChildWindows(Some(hwnd), Some(warm_child), LPARAM(0));
        }
        if let Ok(root) = unsafe { automation.ElementFromHandle(hwnd) } {
            if let Ok(condition) = unsafe { automation.CreateTrueCondition() } {
                let _ = unsafe { root.FindAll(TreeScope_Children, &condition) };
            }
        }
    }

    unsafe fn request_accessible(hwnd: HWND) {
        let mut result = 0usize;
        let _ = unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_GETOBJECT,
                WPARAM(0),
                LPARAM(OBJID_CLIENT.0 as isize),
                SMTO_ABORTIFHUNG,
                100,
                Some(&mut result),
            )
        };
    }

    // ---------- UIA ----------

    fn bstr_string(value: windows::core::Result<BSTR>) -> String {
        value.map(|text| text.to_string()).unwrap_or_default()
    }

    /// 元素属性读取器：`cached == true` 时全部走 `FindAllBuildCache` 的批量缓存，
    /// 否则逐属性跨进程调用。
    ///
    /// 为什么要有两档：缓存把「每个元素 11 次 IPC」压成一次批量返回——2026-10-22
    /// 实测 WorkBuddy（Electron，约 700 个元素）整条扫描 1.6–2.2 s 全部消耗在这些
    /// 往返上。但部分 UIA provider 会忽略缓存请求（缓存属性成批为空），因此调用方
    /// 必须在缓存结果「关键属性全空」时整批回退到非缓存路径。
    struct ElementProbe<'a> {
        element: &'a IUIAutomationElement,
        cached: bool,
    }

    impl<'a> ElementProbe<'a> {
        fn new(element: &'a IUIAutomationElement, cached: bool) -> Self {
            Self { element, cached }
        }

        fn rect(&self) -> windows::core::Result<RECT> {
            unsafe {
                if self.cached {
                    self.element.CachedBoundingRectangle()
                } else {
                    self.element.CurrentBoundingRectangle()
                }
            }
        }

        fn automation_id(&self) -> String {
            bstr_string(unsafe {
                if self.cached {
                    self.element.CachedAutomationId()
                } else {
                    self.element.CurrentAutomationId()
                }
            })
        }

        fn class_name(&self) -> String {
            bstr_string(unsafe {
                if self.cached {
                    self.element.CachedClassName()
                } else {
                    self.element.CurrentClassName()
                }
            })
        }

        fn name(&self) -> String {
            bstr_string(unsafe {
                if self.cached {
                    self.element.CachedName()
                } else {
                    self.element.CurrentName()
                }
            })
        }

        fn enabled(&self) -> Option<bool> {
            let result = unsafe {
                if self.cached {
                    self.element.CachedIsEnabled()
                } else {
                    self.element.CurrentIsEnabled()
                }
            };
            result.ok().map(|flag| flag.as_bool())
        }

        fn keyboard_focusable(&self) -> Option<bool> {
            let result = unsafe {
                if self.cached {
                    self.element.CachedIsKeyboardFocusable()
                } else {
                    self.element.CurrentIsKeyboardFocusable()
                }
            };
            result.ok().map(|flag| flag.as_bool())
        }

        fn is_password(&self) -> Option<bool> {
            let result = unsafe {
                if self.cached {
                    self.element.CachedIsPassword()
                } else {
                    self.element.CurrentIsPassword()
                }
            };
            result.ok().map(|flag| flag.as_bool())
        }

        fn has_keyboard_focus(&self) -> Option<bool> {
            let result = unsafe {
                if self.cached {
                    self.element.CachedHasKeyboardFocus()
                } else {
                    self.element.CurrentHasKeyboardFocus()
                }
            };
            result.ok().map(|flag| flag.as_bool())
        }

        fn control_type(&self) -> Option<UIA_CONTROLTYPE_ID> {
            let result = unsafe {
                if self.cached {
                    self.element.CachedControlType()
                } else {
                    self.element.CurrentControlType()
                }
            };
            result.ok()
        }

        /// 只区分我们关心的四类；其余统一为 "Other"（不参与候选判定）。
        fn control_type_name(&self) -> String {
            match self.control_type() {
                Some(id) if id == UIA_EditControlTypeId => "Edit".to_owned(),
                Some(id) if id == UIA_DocumentControlTypeId => "Document".to_owned(),
                Some(id) if id == UIA_GroupControlTypeId => "Group".to_owned(),
                _ => "Other".to_owned(),
            }
        }

        fn has_pattern(&self, pattern: UIA_PATTERN_ID) -> bool {
            unsafe {
                if self.cached {
                    self.element.GetCachedPattern(pattern).is_ok()
                } else {
                    self.element.GetCurrentPattern(pattern).is_ok()
                }
            }
        }

        fn read_only(&self) -> Option<bool> {
            let pattern = unsafe {
                if self.cached {
                    self.element.GetCachedPattern(UIA_ValuePatternId)
                } else {
                    self.element.GetCurrentPattern(UIA_ValuePatternId)
                }
            }
            .ok()?;
            let value: IUIAutomationValuePattern = pattern.cast().ok()?;
            unsafe { value.CurrentIsReadOnly() }
                .ok()
                .map(|flag| flag.as_bool())
        }
    }

    /// 合法矩形：坐标在 ±1e6 内且右下大于左上（饱和值/离屏空矩形会被拒绝）。
    fn sane_rect(rect: &RECT) -> Option<(f64, f64, f64, f64)> {
        let values = [rect.left, rect.top, rect.right, rect.bottom];
        if values.iter().any(|value| value.abs() > MAX_ABS_COORDINATE) {
            return None;
        }
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return None;
        }
        Some((
            f64::from(rect.left),
            f64::from(rect.top),
            f64::from(rect.right - rect.left),
            f64::from(rect.bottom - rect.top),
        ))
    }

    /// 批量缓存请求：一次往返取回候选判定需要的全部属性。
    fn build_cache_request(automation: &IUIAutomation) -> Option<IUIAutomationCacheRequest> {
        let cache = unsafe { automation.CreateCacheRequest() }.ok()?;
        let properties = [
            UIA_BoundingRectanglePropertyId,
            UIA_AutomationIdPropertyId,
            UIA_ClassNamePropertyId,
            UIA_NamePropertyId,
            UIA_ControlTypePropertyId,
            UIA_IsEnabledPropertyId,
            UIA_IsKeyboardFocusablePropertyId,
            UIA_IsPasswordPropertyId,
            UIA_HasKeyboardFocusPropertyId,
        ];
        for property in properties {
            unsafe { cache.AddProperty(property) }.ok()?;
        }
        unsafe { cache.AddPattern(UIA_ValuePatternId) }.ok()?;
        unsafe { cache.AddPattern(UIA_TextPatternId) }.ok()?;
        Some(cache)
    }

    /// `VT_I4` 变体（UIA 的属性条件用整型枚举值）。
    fn variant_i4(value: i32) -> VARIANT {
        let mut variant = VARIANT::default();
        unsafe {
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_I4;
            inner.Anonymous.lVal = value;
        }
        variant
    }

    /// `VT_BOOL` 变体（UIA 的布尔属性条件）。
    fn variant_bool(value: bool) -> VARIANT {
        let mut variant = VARIANT::default();
        unsafe {
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_BOOL;
            inner.Anonymous.boolVal = if value { VARIANT_TRUE } else { VARIANT_FALSE };
        }
        variant
    }

    /// 候选类型条件：让 provider 只返回「可能是文本输入」的元素。
    ///
    /// 为什么需要：`TrueCondition` 会让 Chromium/Electron 的 UIA provider 把整棵树
    /// 都序列化过来（2026-10-22 实测 WorkBuddy 464 个元素、单次 `FindAll` 约 1.4 s）。
    ///
    /// 条件必须**覆盖候选判定的全部分支**，否则会悄悄缩小候选集：
    /// `passes_hard_gate` 接受 `is_text_control()`（Edit/Document）、
    /// `has_text_pattern`（**任意类型**，Chrome 里就有 `ControlType=Other` 的输入框）
    /// 或 `has_editor_semantics`（class/name 命中编辑器词）。因此这里取
    /// 「带 TextPattern」∪「Edit/Document/Group」，前两项已覆盖实测全部正例；
    /// 纯语义命中的元素交给调用方的 0 候选回退（全树）兜底。
    fn candidate_condition(automation: &IUIAutomation) -> Option<IUIAutomationCondition> {
        let mut conditions = Vec::with_capacity(4);
        let text_value = variant_bool(true);
        conditions.push(Some(
            unsafe {
                automation
                    .CreatePropertyCondition(UIA_IsTextPatternAvailablePropertyId, &text_value)
            }
            .ok()?,
        ));
        for control_type in [
            UIA_EditControlTypeId,
            UIA_DocumentControlTypeId,
            UIA_GroupControlTypeId,
        ] {
            let value = variant_i4(control_type.0);
            conditions.push(Some(
                unsafe { automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &value) }
                    .ok()?,
            ));
        }
        unsafe { automation.CreateOrConditionFromNativeArray(&conditions) }.ok()
    }

    struct ScanResult {
        candidates: Vec<FocusCandidate>,
        /// 与 `candidates` 同序的元素引用：`focus_target` 直接用它 `SetFocus`，
        /// 不再第二次全树扫描（原先这条重复扫描让一次尝试翻倍）。
        elements: Vec<IUIAutomationElement>,
        /// 本次遍历看到的元素总数（冷树判据的一部分）。
        total: usize,
    }

    /// 候选门槛档位：自动选择用硬门槛（排除地址栏等），
    /// 「按记录聚焦」用学习门槛（记录来自用户主动点击，排除词不参与）。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CandidateGate {
        Auto,
        Recorded,
    }

    impl CandidateGate {
        fn accepts(self, candidate: &FocusCandidate) -> bool {
            match self {
                Self::Auto => candidate.passes_hard_gate(),
                Self::Recorded => candidate.passes_learning_gate(),
            }
        }
    }

    fn snapshot_candidate(
        probe: &ElementProbe<'_>,
        window_frame: Option<(f64, f64, f64, f64)>,
    ) -> FocusCandidate {
        let rect = probe.rect().ok();
        let bounds = rect.as_ref().and_then(sane_rect);
        let normalized_rect = crate::focus::NormalizedRect::from_bounds(bounds, window_frame);
        FocusCandidate {
            control_type: probe.control_type_name(),
            automation_id: probe.automation_id(),
            class_name: probe.class_name(),
            name: probe.name(),
            window_title: String::new(),
            context_tokens: Vec::new(),
            normalized_rect,
            enabled: probe.enabled().unwrap_or(true),
            keyboard_focusable: probe.keyboard_focusable().unwrap_or(false),
            is_password: probe.is_password().unwrap_or(false),
            read_only: probe.read_only(),
            has_text_pattern: probe.has_pattern(UIA_TextPatternId),
            focused: probe.has_keyboard_focus().unwrap_or(false),
        }
    }

    /// 一次元素枚举的结果（含「缓存是否被 provider 忽略」的判据）。
    struct ScanBatch {
        candidates: Vec<FocusCandidate>,
        elements: Vec<IUIAutomationElement>,
        /// 关键属性（矩形/ID/类名/名称）全空的元素数。
        blank: usize,
        total: usize,
    }

    fn collect_from(
        elements: &IUIAutomationElementArray,
        window_frame: Option<(f64, f64, f64, f64)>,
        gate: CandidateGate,
        cached: bool,
    ) -> ScanBatch {
        let count = unsafe { elements.Length() }.unwrap_or(0).max(0) as usize;
        let mut candidates = Vec::new();
        let mut kept = Vec::new();
        let mut blank = 0usize;
        for index in 0..count.min(MAX_TRAVERSAL_ELEMENTS) {
            let Ok(element) = (unsafe { elements.GetElement(index as i32) }) else {
                continue;
            };
            let probe = ElementProbe::new(&element, cached);
            let candidate = snapshot_candidate(&probe, window_frame);
            if candidate.automation_id.is_empty()
                && candidate.class_name.is_empty()
                && candidate.name.is_empty()
                && candidate.normalized_rect.is_none()
            {
                blank += 1;
            }
            if gate.accepts(&candidate) {
                candidates.push(candidate);
                kept.push(element);
            }
        }
        ScanBatch {
            candidates,
            elements: kept,
            blank,
            total: count,
        }
    }

    /// 一次枚举：优先用批量缓存；provider 忽略缓存、或树明显是冷占位节点时
    /// 回退非缓存。
    fn find_all(
        automation: &IUIAutomation,
        root: &IUIAutomationElement,
        condition: &IUIAutomationCondition,
        window_frame: Option<(f64, f64, f64, f64)>,
        gate: CandidateGate,
        level: &'static str,
        attempt: usize,
        started: Instant,
    ) -> Option<ScanBatch> {
        let mut fallback_batch = None;
        if let Some(cache) = build_cache_request(automation) {
            if let Ok(array) =
                unsafe { root.FindAllBuildCache(TreeScope_Descendants, condition, &cache) }
            {
                let batch = collect_from(&array, window_frame, gate, true);
                // provider 忽略缓存请求时，缓存属性会成批为空：整批作废、回退非缓存。
                if batch.total == 0 || batch.blank < batch.total {
                    let cold = crate::focus::scan_tree_is_cold(batch.total, batch.candidates.len());
                    crate::ble::gatt_note(format!(
                        "focus_scan level={level} path=cache attempt={attempt} elements={} \
                         candidates={} blank={} cold={cold} elapsed_ms={}",
                        batch.total,
                        batch.candidates.len(),
                        batch.blank,
                        started.elapsed().as_millis()
                    ));
                    if !cold {
                        return Some(batch);
                    }
                    crate::ble::gatt_note(format!(
                        "focus_scan level={level} path=cache attempt={attempt} \
                         terminal_result=fallback reason=cold_tree elements={} elapsed_ms={}",
                        batch.total,
                        started.elapsed().as_millis()
                    ));
                    fallback_batch = Some(batch);
                } else {
                    crate::ble::gatt_note(format!(
                        "focus_scan level={level} path=cache attempt={attempt} \
                         terminal_result=fallback reason=cache_ignored elements={} elapsed_ms={}",
                        batch.total,
                        started.elapsed().as_millis()
                    ));
                }
            }
        }
        let elements = match unsafe { root.FindAll(TreeScope_Descendants, condition) } {
            Ok(elements) => elements,
            Err(_) => return fallback_batch,
        };
        let batch = collect_from(&elements, window_frame, gate, false);
        crate::ble::gatt_note(format!(
            "focus_scan level={level} path=plain attempt={attempt} elements={} candidates={} \
             blank={} elapsed_ms={}",
            batch.total,
            batch.candidates.len(),
            batch.blank,
            started.elapsed().as_millis()
        ));
        Some(batch)
    }

    /// 单次扫描：先窄条件后全树。抽出来供「冷树重试」复用。
    fn scan_once(
        automation: &IUIAutomation,
        root: &IUIAutomationElement,
        window_frame: Option<(f64, f64, f64, f64)>,
        gate: CandidateGate,
        attempt: usize,
    ) -> Result<ScanResult, FocusFailure> {
        let started = Instant::now();
        // 第一档：类型条件（provider 侧过滤）。已实测的全部正例都落在
        // Edit/Document/Group 里，所以正常情况在这一档就结束。
        if let Some(narrow) = candidate_condition(automation) {
            if let Some(batch) = find_all(
                automation,
                root,
                &narrow,
                window_frame,
                gate,
                "narrow",
                attempt,
                started,
            ) {
                if !batch.candidates.is_empty() {
                    return Ok(ScanResult {
                        candidates: batch.candidates,
                        elements: batch.elements,
                        total: batch.total,
                    });
                }
            }
        }
        // 第二档：全树。自定义控件可能不落在上面三种类型里，这里只兜正确性。
        let wide =
            unsafe { automation.CreateTrueCondition() }.map_err(|_| FocusFailure::NotAccessible)?;
        let batch = find_all(
            automation,
            root,
            &wide,
            window_frame,
            gate,
            "wide",
            attempt,
            started,
        )
        .ok_or(FocusFailure::NotAccessible)?;
        Ok(ScanResult {
            candidates: batch.candidates,
            elements: batch.elements,
            total: batch.total,
        })
    }

    fn collect_candidates(
        session: &Session,
        hwnd: HWND,
        gate: CandidateGate,
    ) -> Result<ScanResult, FocusFailure> {
        let automation = &session.automation;
        let class = window_class_name(hwnd);
        let chromium = class.starts_with("Chrome_WidgetWin_");
        crate::ble::gatt_note(format!(
            "focus_scan_target hwnd={:#x} class={class} chromium={chromium} gate={}",
            hwnd.0 as isize,
            match gate {
                CandidateGate::Auto => "auto",
                CandidateGate::Recorded => "recorded",
            }
        ));
        let root = unsafe { automation.ElementFromHandle(hwnd) }
            .map_err(|_| FocusFailure::NotAccessible)?;
        let window_frame = unsafe { root.CurrentBoundingRectangle() }
            .ok()
            .and_then(|rect| sane_rect(&rect));
        let attempts = if chromium { 3 } else { 1 };
        let mut last = ScanResult {
            candidates: Vec::new(),
            elements: Vec::new(),
            total: 0,
        };
        for attempt in 0..attempts {
            let scan = scan_once(automation, &root, window_frame, gate, attempt)?;
            let cold = crate::focus::scan_tree_is_cold(scan.total, scan.candidates.len());
            let retrying = cold && attempt + 1 < attempts;
            if !scan.candidates.is_empty() {
                return Ok(scan);
            }
            last = scan;
            if !retrying {
                break;
            }
            crate::ble::gatt_note(format!(
                "focus_scan_retry attempt={attempt} reason=cold_tree elements={} candidates=0",
                last.total
            ));
            warm_accessibility(automation, hwnd);
            std::thread::sleep(Duration::from_millis(150));
        }
        Ok(last)
    }

    /// 扫描目标进程当前窗口的可编辑候选（语义快照，不含输入内容）。
    pub fn scan_candidates(pid: u32) -> Result<Vec<FocusCandidate>, FocusFailure> {
        if !process_alive(pid) {
            return Err(FocusFailure::TargetExited);
        }
        let Some((hwnd, _)) = target_window(pid) else {
            return Err(FocusFailure::TargetExited);
        };
        // HWND 未实现 Send：只把裸指针宽度值送过线程边界。
        let raw = hwnd.0 as isize;
        submit(move |session| {
            collect_candidates(session, hwnd_from_raw(raw), CandidateGate::Auto)
                .map(|result| result.candidates)
        })
        .unwrap_or(Err(FocusFailure::NotAccessible))
    }

    fn hwnd_from_raw(raw: isize) -> HWND {
        HWND(raw as *mut core::ffi::c_void)
    }

    /// 常用语插入前的只读校验：目标进程里**当前持有键盘焦点**的元素是不是可用的输入目标。
    ///
    /// 与「聚焦输入框」共用同一硬门槛（[`FocusCandidate::passes_hard_gate`]）：可用、可聚焦、
    /// 非密码框、非只读、语义不含敏感/排除词，且焦点元素确实属于目标进程。
    /// 只读：不修改任何目标状态，也不读取任何输入内容。
    ///
    /// 返回 `Ok(false)` = 当前没有可用输入目标（无焦点、焦点在别的进程、只读或密码框）；
    /// `Err` = 目标进程不存在或 UIA 不可用（应用没有无障碍树）。
    pub fn focused_input_available(pid: u32) -> Result<bool, FocusFailure> {
        if !process_alive(pid) {
            return Err(FocusFailure::TargetExited);
        }
        let Some((hwnd, _)) = target_window(pid) else {
            return Err(FocusFailure::TargetExited);
        };
        let raw = hwnd.0 as isize;
        submit(move |session| {
            let hwnd = hwnd_from_raw(raw);
            let Ok(root) = (unsafe { session.automation.ElementFromHandle(hwnd) }) else {
                return Ok(false);
            };
            let window_frame = unsafe { root.CurrentBoundingRectangle() }
                .ok()
                .and_then(|rect| sane_rect(&rect));
            let Ok(focused) = (unsafe { session.automation.GetFocusedElement() }) else {
                return Ok(false);
            };
            let focused_pid = unsafe { focused.CurrentProcessId() }
                .map(|value| value as u32)
                .unwrap_or(0);
            if focused_pid != pid {
                // 焦点在别的应用（例如用户已经切走）：不得把文本插到别处。
                return Ok(false);
            }
            let candidate = snapshot_candidate(&ElementProbe::new(&focused, false), window_frame);
            Ok(candidate.passes_hard_gate())
        })
        .unwrap_or(Err(FocusFailure::NotAccessible))
    }

    /// 读回判定：目标元素当前是否持有键盘焦点。
    fn element_has_focus(session: &Session, element: &IUIAutomationElement) -> bool {
        if unsafe { element.CurrentHasKeyboardFocus() }
            .map(|flag| flag.as_bool())
            .unwrap_or(false)
        {
            return true;
        }
        let Ok(focused) = (unsafe { session.automation.GetFocusedElement() }) else {
            return false;
        };
        let (Ok(left), Ok(right)) = (unsafe { element.GetRuntimeId() }, unsafe {
            focused.GetRuntimeId()
        }) else {
            return false;
        };
        let matches = unsafe { session.automation.CompareRuntimeIds(left, right) }
            .map(|flag| flag.as_bool())
            .unwrap_or(false);
        // UIA 每次调用都返回新数组，所有权归调用方（MS Learn: GetRuntimeId）。
        unsafe {
            let _ = SafeArrayDestroy(left);
            let _ = SafeArrayDestroy(right);
        }
        matches
    }

    /// 扫描 + 选择 + 聚焦 + 读回，全部在同一次 UIA 调用里完成（避免索引漂移）。
    pub fn focus_target(pid: u32, choice: FocusChoice) -> FocusAttempt {
        if !process_alive(pid) {
            return FocusAttempt::TargetExited;
        }
        let Some((hwnd, _)) = target_window(pid) else {
            return FocusAttempt::TargetExited;
        };
        // 「按记录聚焦」的候选与自动选择用不同门槛（记录来自用户主动点击）：
        // 检索与写入共用同一档，否则选中的序号与要聚焦的元素对不上。
        let gate = match &choice {
            FocusChoice::Recorded(_) => CandidateGate::Recorded,
            _ => CandidateGate::Auto,
        };
        let raw = hwnd.0 as isize;
        let result = submit(move |session| {
            let hwnd = hwnd_from_raw(raw);
            let scan = match collect_candidates(session, hwnd, gate) {
                Ok(scan) => scan,
                Err(reason) => return FocusAttempt::Failed(reason),
            };
            let candidate_count = scan.candidates.len();
            let chosen = match &choice {
                FocusChoice::Recorded(target) => best_recorded_index(&scan.candidates, target),
                FocusChoice::BestComposer => best_composer_index(&scan.candidates),
                FocusChoice::Index(index) => {
                    (scan.candidates.get(*index).is_some()).then_some(*index)
                }
            };
            let Some(index) = chosen else {
                return FocusAttempt::NoCandidate {
                    candidates: candidate_count,
                };
            };
            // 直接用扫描阶段拿到的元素引用聚焦：不再二次 `FindAll` + 逐元素重取属性
            // （2026-10-22 实测，那次重复扫描让一次尝试的成本翻倍）。
            let Some(element) = scan.elements.get(index).cloned() else {
                return FocusAttempt::NoCandidate {
                    candidates: candidate_count,
                };
            };
            if unsafe { element.SetFocus() }.is_err() {
                return FocusAttempt::NoCandidate {
                    candidates: candidate_count,
                };
            }
            let focused = element_has_focus(session, &element);
            if focused {
                FocusAttempt::Focused {
                    candidates: candidate_count,
                }
            } else {
                FocusAttempt::NoCandidate {
                    candidates: candidate_count,
                }
            }
        });
        result.unwrap_or(FocusAttempt::Failed(FocusFailure::NotAccessible))
    }

    /// 通用路径的薄封装（前台应用 + 最佳 composer 候选）。
    pub fn focus_best(pid: u32, target: Option<RecordedFocusTarget>) -> FocusAttempt {
        let choice = match target {
            Some(target) => FocusChoice::Recorded(target),
            None => FocusChoice::BestComposer,
        };
        focus_target(pid, choice)
    }

    /// 采集当前系统焦点元素（「学习输入框」用）；要求属于指定进程、通过候选硬门槛
    /// 且能形成可比对的语义特征（否则返回 `None`，UI 报「没学到可用的输入框」）。
    ///
    /// 焦点元素不可用时（典型：Electron 应用把焦点报成顶层 `Window`，`focusable=false`），
    /// 退回到「光标所在的输入框候选」——即用户点击的位置（2026-10-19 现场：
    /// WorkBuddy 点击输入框后焦点仍是顶层窗口，学习一直 `no_candidate`）。
    pub fn capture_focused_target(pid: u32) -> Option<RecordedFocusTarget> {
        submit(move |session| {
            capture_focused_element(session, pid)
                .or_else(|| capture_target_under_cursor(session, pid))
        })
        .flatten()
    }

    /// 系统焦点元素（严格：必须属于目标进程、可输入、有可比特征）。
    fn capture_focused_element(session: &Session, pid: u32) -> Option<RecordedFocusTarget> {
        let focused = unsafe { session.automation.GetFocusedElement() }.ok()?;
        let element_pid = unsafe { focused.CurrentProcessId() }.ok()? as u32;
        if element_pid != pid {
            return None;
        }
        // 学习必须命中「能输入」的元素：只读网页根、按钮、密码框一律不记。
        // 门槛用学习档：用户主动点击的元素就是目标（地址栏这类排除词只在自动
        // 选择路径生效，见 `FocusCandidate::passes_learning_gate`）。
        let focused_candidate = snapshot_candidate(&ElementProbe::new(&focused, false), None);
        if !focused_candidate.passes_learning_gate() {
            return None;
        }
        let target = RecordedFocusTarget {
            control_type: focused_candidate.control_type,
            automation_id: focused_candidate.automation_id,
            class_name: focused_candidate.class_name,
            name: focused_candidate.name,
            window_title: String::new(),
            normalized_rect: None,
            context_tokens: Vec::new(),
        };
        target.normalized()
    }

    /// 光标命中兜底：扫目标窗口的输入候选，取「矩形包含光标」的那一个。
    ///
    /// 不依赖应用是否把键盘焦点暴露给 UIA，也不依赖 `ElementFromPoint`
    /// （Chromium 的 `ElementFromPoint` 可能只返回 render host）。
    fn capture_target_under_cursor(session: &Session, pid: u32) -> Option<RecordedFocusTarget> {
        let (cursor_x, cursor_y) = physical_cursor()?;
        let (hwnd, _) = target_window(pid)?;
        let root = unsafe { session.automation.ElementFromHandle(hwnd) }.ok()?;
        let (wx, wy, ww, wh) = unsafe { root.CurrentBoundingRectangle() }
            .ok()
            .and_then(|rect| sane_rect(&rect))?;
        if cursor_x < wx || cursor_y < wy || cursor_x > wx + ww || cursor_y > wy + wh {
            crate::ble::gatt_note(format!(
                "focus_learn_cursor terminal_result=skipped reason=outside_window pid={pid} \
                 hwnd={:#x} class={} window={}x{}",
                hwnd.0 as isize,
                window_class_name(hwnd),
                ww,
                wh
            ));
            return None;
        }
        let x = (cursor_x - wx) / ww;
        let y = (cursor_y - wy) / wh;
        let scan = collect_candidates(session, hwnd, CandidateGate::Recorded).ok()?;
        let mut hits: Vec<&FocusCandidate> = scan
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.normalized_rect.is_some_and(|rect| {
                    x >= rect.x
                        && x <= rect.x + rect.width
                        && y >= rect.y
                        && y <= rect.y + rect.height
                })
            })
            .collect();
        crate::ble::gatt_note(format!(
            "focus_learn_cursor phase=hit_test pid={pid} hwnd={:#x} class={} \
             normalized_x={:.3} normalized_y={:.3} candidates={} hits={} window={}x{}",
            hwnd.0 as isize,
            window_class_name(hwnd),
            x,
            y,
            scan.candidates.len(),
            hits.len(),
            ww,
            wh
        ));
        // 优先「本来就有焦点」的，其次面积最小的（更具体的输入框）。
        hits.sort_by(|a, b| {
            b.focused.cmp(&a.focused).then_with(|| {
                let area = |candidate: &FocusCandidate| {
                    candidate
                        .normalized_rect
                        .map(|rect| rect.width * rect.height)
                        .unwrap_or(f64::MAX)
                };
                area(a).total_cmp(&area(b))
            })
        });
        let chosen = hits.first().copied()?;
        let normalized = RecordedFocusTarget {
            control_type: chosen.control_type.clone(),
            automation_id: chosen.automation_id.clone(),
            class_name: chosen.class_name.clone(),
            name: chosen.name.clone(),
            window_title: String::new(),
            normalized_rect: chosen.normalized_rect,
            context_tokens: Vec::new(),
        }
        .normalized();
        normalized
    }

    /// 物理光标位置（UIA 工作线程固定 Per-Monitor V2，与元素矩形同坐标系）。
    fn physical_cursor() -> Option<(f64, f64)> {
        let mut point = POINT::default();
        unsafe { GetPhysicalCursorPos(&mut point) }.ok()?;
        Some((f64::from(point.x), f64::from(point.y)))
    }

    /// 一次聚焦尝试的结果（对上层可映射为 `AttemptResult`）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FocusAttempt {
        Focused { candidates: usize },
        NoCandidate { candidates: usize },
        Failed(FocusFailure),
        TargetExited,
    }

    impl FocusAttempt {
        pub fn into_attempt_result(self) -> AttemptResult {
            match self {
                Self::Focused { .. } => AttemptResult::Focused,
                Self::NoCandidate { .. } => AttemptResult::NoCandidate,
                Self::TargetExited => AttemptResult::Fatal(FocusFailure::TargetExited),
                Self::Failed(reason) => AttemptResult::Fatal(reason),
            }
        }
    }

    /// 生产后端：实现 `FocusBackend`，由 `FocusRunner` 的串行队列调用。
    #[derive(Debug, Default)]
    pub struct WindowsFocusBackend;

    impl WindowsFocusBackend {
        pub fn new() -> Self {
            Self
        }
    }

    /// 进程内单调时钟锚点（只用于预算判定与日志耗时）。
    fn service_clock_ms() -> u64 {
        static ANCHOR: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        ANCHOR.get_or_init(Instant::now).elapsed().as_millis() as u64
    }

    impl crate::focus_service::FocusBackend for WindowsFocusBackend {
        fn now_ms(&self) -> u64 {
            service_clock_ms()
        }

        fn sleep(&self, duration: std::time::Duration) {
            std::thread::sleep(duration);
        }

        fn foreground_process_id(&self) -> Option<u32> {
            foreground_process_id()
        }

        fn current_process_id(&self) -> u32 {
            current_process_id()
        }

        fn process_alive(&self, pid: u32) -> bool {
            process_alive(pid)
        }

        fn attempt(&self, pid: u32, choice: &crate::focus::FocusChoice) -> AttemptResult {
            focus_target(pid, choice.clone()).into_attempt_result()
        }

        fn sample_learning_target(&self) -> Option<RecordedFocusTarget> {
            let pid = foreground_process_id()?;
            if pid == current_process_id() {
                // 用户在学习窗口内点了本应用自己的界面：不记，等继续点目标应用。
                return None;
            }
            capture_focused_target(pid)
        }
    }

    /// 学习输入框：把「目标进程当前焦点元素」当作文档记录（只读语义，不读内容）。
    pub fn capture_for_process(pid: u32) -> Option<RecordedFocusTarget> {
        capture_focused_target(pid)
    }

    /// 供诊断：把 `HWND` 转成指针宽度（跨线程传递用）。
    ///
    /// 前台窗口属于目标进程时直接返回它：滚动语义是「滚我正在看的窗口」，
    /// 这比按面积挑主窗口更准确（弹窗/多窗口应用）。
    pub fn window_of_process(pid: u32) -> Option<isize> {
        let foreground = unsafe { GetForegroundWindow() };
        if !foreground.is_invalid() {
            let mut window_pid = 0u32;
            unsafe { GetWindowThreadProcessId(foreground, Some(&mut window_pid)) };
            if window_pid == pid {
                return Some(foreground.0 as isize);
            }
        }
        target_window(pid).map(|(hwnd, _)| hwnd.0 as isize)
    }

    /// 供诊断使用：当前前台窗口标题长度（不落内容）。
    pub fn foreground_title_len() -> usize {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_invalid() {
            return 0;
        }
        let mut buffer = [0u16; 512];
        let len =
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(hwnd, &mut buffer) };
        len.max(0) as usize
    }

    /// 占位：确保 `PCWSTR` 等导入在仅用于诊断时不触发未使用告警。
    #[allow(dead_code)]
    fn _keep_imports(_: PCWSTR, _: &IUIAutomationElementArray) {}
}

#[cfg(windows)]
pub use imp::{
    capture_focused_target, capture_for_process, current_process_id, focus_best, focus_target,
    focused_input_available, foreground_executable_path, foreground_process_id,
    foreground_title_len, process_alive, scan_candidates, window_of_process, FocusAttempt,
    WindowsFocusBackend,
};

/// 复用共享 UIA 工作线程（Per-Monitor V2、MTA）执行只读 UIA 任务的入口。
#[cfg(windows)]
pub(crate) use imp::{
    warm_accessibility, window_class_name, with_automation, with_automation_timeout,
};

/// 非 Windows 平台不提供 UIA 后端；保持模块可编译以便纯逻辑单测。
#[cfg(not(windows))]
pub fn unsupported() -> FocusFailure {
    FocusFailure::NotAccessible
}
