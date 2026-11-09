//! Native transport and queue installation only. Core owns jobs and billing.
use crate::{
    CommandError,
    campus_printer::{CampusPrinterSetup, Setup},
    core_ports,
};
use app_core::campus_printer::{
    PrinterAccount,
    ipp::{CampusPrintQueue, MAX_DOCUMENT_BYTES},
};
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    os::windows::process::CommandExt,
    path::PathBuf,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};
use tauri::Manager;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError},
        Graphics::{
            Gdi::DEVMODEW,
            Printing::{
                ClosePrinter, EnumJobsW, JOB_CONTROL_DELETE, JOB_INFO_1W, JOB_STATUS_BLOCKED_DEVQ,
                JOB_STATUS_COMPLETE, JOB_STATUS_DELETED, JOB_STATUS_DELETING, JOB_STATUS_ERROR,
                JOB_STATUS_OFFLINE, JOB_STATUS_PAPEROUT, JOB_STATUS_PAUSED, JOB_STATUS_PRINTED,
                JOB_STATUS_PRINTING, JOB_STATUS_RENDERING_LOCALLY, JOB_STATUS_RESTART,
                JOB_STATUS_RETAINED, JOB_STATUS_SPOOLING, JOB_STATUS_USER_INTERVENTION,
                OpenPrinterW, PRINTER_ACCESS_ADMINISTER, PRINTER_DEFAULTSW, PRINTER_HANDLE,
                SetJobW,
            },
        },
        System::Threading::{GetExitCodeProcess, WaitForSingleObject},
        UI::{
            Shell::{
                SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
            },
            WindowsAndMessaging::SW_HIDE,
        },
    },
    core::{HSTRING, PCWSTR, w},
};

#[derive(Serialize, Deserialize)]
struct Endpoint {
    port: u16,
    token: uuid::Uuid,
    #[serde(default)]
    private_token: bool,
}
impl Endpoint {
    fn path(&self) -> String {
        format!("/ipp/{}", self.token)
    }
    fn http_uri(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path())
    }
    fn ipp_uri(&self) -> String {
        format!("ipp://127.0.0.1:{}{}", self.port, self.path())
    }
    fn marker(&self) -> String {
        format!(
            "IsctCompanion IPP v2 {}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(self.http_uri().as_bytes()))
        )
    }
}
struct Running {
    endpoint: Endpoint,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    cleanup_task: tokio::task::JoinHandle<()>,
}
#[derive(Default)]
pub(crate) struct PrintServer(Mutex<Option<Running>>);
#[derive(Clone)]
struct HttpState {
    queue: Arc<CampusPrintQueue>,
    requests: Arc<tokio::sync::Semaphore>,
}

async fn receive(State(state): State<HttpState>, request: Request) -> Response {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        != Some("application/ipp")
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    // No CORS and an unguessable per-user route; the Windows queue is also ACLed.
    let Ok(_permit) = state.requests.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let body = match tokio::time::timeout(
        Duration::from_secs(30),
        to_bytes(request.into_body(), MAX_DOCUMENT_BYTES + 65536),
    )
    .await
    {
        Ok(Ok(body)) => body,
        _ => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    match state.queue.handle(body).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/ipp")], bytes).into_response(),
        Err(error) => {
            log::warn!("[campus-printer] invalid IPP request: {error}");
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

async fn receiver_ready() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"status":"ready"}"#,
    )
        .into_response()
}

impl PrintServer {
    async fn start(&self, app: &tauri::AppHandle, create: bool) -> Result<(), String> {
        let mut running = self.0.lock().await;
        if running.as_ref().is_some_and(|r| !r.task.is_finished()) {
            return Ok(());
        }
        let config = app
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string())?
            .join("campus-printer-endpoint.json");
        let mut saved = match std::fs::read(&config) {
            Ok(bytes) => match serde_json::from_slice::<Endpoint>(&bytes) {
                Ok(endpoint) => Some(endpoint),
                Err(_) if create => None,
                Err(_) => {
                    return Err("Invalid printer configuration; use Repair system printer".into());
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        if saved.is_none() && !create {
            return Ok(());
        }
        // The Windows queue stores this URL. Resume must keep the configured
        // port; silently switching to an ephemeral port leaves the queue
        // sending to the old one. Only an explicit repair may choose a new port.
        let saved_port = saved.as_ref().map_or(0, |s| s.port);
        let listener = match bind_receiver(saved_port).await {
            Ok(listener) => listener,
            Err(error) if create && saved_port != 0 => {
                log::warn!(
                    "[campus-printer] saved receiver port is unavailable; repair will use a new port: {error}"
                );
                bind_receiver(0)
                    .await
                    .map_err(|fallback| fallback.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        };
        let mut endpoint = saved.take().unwrap_or_else(|| Endpoint {
            port: 0,
            token: uuid::Uuid::new_v4(),
            private_token: true,
        });
        // Keep the installed URL stable during automatic resume. Rotate the
        // legacy public token only as part of the explicit queue repair below.
        if create && !endpoint.private_token {
            endpoint.token = uuid::Uuid::new_v4();
            endpoint.private_token = true;
        }
        endpoint.port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let app_for_report = app.clone();
        let state = HttpState {
            queue: CampusPrintQueue::new_with_delivery_reporter(
                core_ports::core(app),
                endpoint.ipp_uri(),
                format!("urn:uuid:{}", endpoint.token),
                Arc::new(move |id, message| {
                    let app = app_for_report.clone();
                    tauri::async_runtime::spawn(async move {
                        let body = format!("作业 {id}：{message}");
                        crate::commands::notifications::notify_campus_printer_failure(&app, body)
                            .await;
                    });
                }),
            ),
            requests: Arc::new(tokio::sync::Semaphore::new(4)),
        };
        let router = Router::new()
            .route("/", get(receiver_ready))
            .route(&endpoint.path(), get(receiver_ready).post(receive))
            .route(
                &format!("{}/jobs/{{id}}", endpoint.path()),
                get(receiver_ready).post(receive),
            )
            .with_state(state);
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
            {
                log::error!("[campus-printer] IPP listener stopped: {error}");
            }
        });
        let cleanup_stop = stop.clone();
        let app = app.clone();
        let cleanup_task = tokio::spawn(async move {
            cleanup_queue(app, cleanup_stop).await;
        });
        *running = Some(Running {
            endpoint,
            stop,
            task,
            cleanup_task,
        });
        Ok(())
    }
    async fn persist_endpoint(&self, app: &tauri::AppHandle) -> Result<(), String> {
        let running = self.0.lock().await;
        let endpoint = running
            .as_ref()
            .filter(|r| !r.task.is_finished())
            .map(|r| &r.endpoint)
            .ok_or("Printer receiver did not start")?;
        let config = app
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string())?
            .join("campus-printer-endpoint.json");
        if let Some(parent) = config.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(
            config,
            serde_json::to_vec(endpoint).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }
    pub(crate) async fn shutdown(&self) {
        if let Some(running) = self.0.lock().await.take() {
            running.stop.cancel();
            let _ = running.task.await;
            let _ = running.cleanup_task.await;
        }
    }
}

const QUEUE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const UNKNOWN_JOB_GRACE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy)]
struct SpoolJob {
    id: u32,
    status: u32,
}

async fn cleanup_queue(app: tauri::AppHandle, stop: CancellationToken) {
    let mut printer_name = None::<String>;
    let mut first_seen = HashMap::<u32, Instant>::new();
    let mut reported = HashSet::<u32>::new();
    let mut interval = tokio::time::interval(QUEUE_POLL_INTERVAL);
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = interval.tick() => {
                if printer_name.is_none() {
                    match identity() {
                        Ok((_, _, name)) => printer_name = Some(name),
                        Err(error) => {
                            log::warn!("[campus-printer] could not identify the Windows queue for cleanup: {error}");
                            continue;
                        }
                    }
                }
                let Some(name) = printer_name.clone() else { continue; };
                let jobs = match tauri::async_runtime::spawn_blocking(move || enum_spool_jobs(&name)).await {
                    Ok(Ok(jobs)) => jobs,
                    Ok(Err(error)) => {
                        log::warn!("[campus-printer] could not inspect print queue: {error}");
                        continue;
                    }
                    Err(error) => {
                        log::warn!("[campus-printer] print queue inspection task failed: {error}");
                        continue;
                    }
                };
                let present: HashSet<u32> = jobs.iter().map(|job| job.id).collect();
                first_seen.retain(|id, _| present.contains(id));
                reported.retain(|id| present.contains(id));
                for job in jobs {
                    let first = *first_seen.entry(job.id).or_insert_with(Instant::now);
                    let elapsed = first.elapsed();
                    let (remove, issue) = spool_job_action(job.status, elapsed);
                    if !remove {
                        continue;
                    }
                    if let Some(issue) = issue.filter(|_| reported.insert(job.id)) {
                        let message = format!("打印作业 {}：{}。现在将从 Windows 打印列表中清除。", job.id, issue);
                        log::warn!("[campus-printer] {message}");
                        crate::commands::notifications::notify_campus_printer_failure(&app, message).await;
                    }
                    let Some(name) = printer_name.clone() else { continue; };
                    let id = job.id;
                    match tauri::async_runtime::spawn_blocking(move || delete_spool_job(&name, id)).await {
                        Ok(Ok(())) => {
                            first_seen.remove(&job.id);
                            log::info!("[campus-printer] removed Windows print job {}", job.id);
                        }
                        Ok(Err(error)) => {
                            let message = format!("无法从 Windows 打印列表清除作业 {}：{error}", job.id);
                            log::warn!("[campus-printer] {message}");
                            if reported.insert(job.id) {
                                crate::commands::notifications::notify_campus_printer_failure(&app, message).await;
                            }
                        }
                        Err(error) => log::warn!("[campus-printer] print job cleanup task failed: {error}"),
                    }
                }
            }
        }
    }
}

fn spool_job_action(status: u32, elapsed: Duration) -> (bool, Option<String>) {
    let failed = status
        & (JOB_STATUS_ERROR
            | JOB_STATUS_OFFLINE
            | JOB_STATUS_PAPEROUT
            | JOB_STATUS_BLOCKED_DEVQ
            | JOB_STATUS_USER_INTERVENTION
            | JOB_STATUS_PAUSED);
    if failed != 0 {
        return (
            true,
            Some(format!("Windows 队列报告错误状态 0x{failed:08X}")),
        );
    }
    if status & (JOB_STATUS_PRINTED | JOB_STATUS_COMPLETE | JOB_STATUS_DELETED) != 0 {
        return (true, None);
    }
    if status & JOB_STATUS_DELETING != 0 {
        return (false, None);
    }
    let active = status
        & (JOB_STATUS_SPOOLING
            | JOB_STATUS_PRINTING
            | JOB_STATUS_RESTART
            | JOB_STATUS_RENDERING_LOCALLY)
        != 0;
    // Zero means normally queued; retained alone does not mean delivery finished.
    if active || status == 0 || status == JOB_STATUS_RETAINED {
        return (false, None);
    }
    let threshold = UNKNOWN_JOB_GRACE;
    if elapsed >= threshold {
        (
            true,
            Some(format!("状态未知或长时间未完成（状态码 0x{status:08X}）")),
        )
    } else {
        (false, None)
    }
}

fn enum_spool_jobs(printer_name: &str) -> Result<Vec<SpoolJob>, String> {
    let name = HSTRING::from(printer_name);
    let defaults = PRINTER_DEFAULTSW {
        pDatatype: windows::core::PWSTR::null(),
        pDevMode: std::ptr::null_mut::<DEVMODEW>(),
        DesiredAccess: PRINTER_ACCESS_ADMINISTER,
    };
    let mut printer = PRINTER_HANDLE::default();
    unsafe { OpenPrinterW(&name, &mut printer, Some(&defaults)) }
        .map_err(|error| format!("OpenPrinterW: {error}"))?;
    let result = (|| {
        let mut needed = 0u32;
        let mut returned = 0u32;
        let _ = unsafe { EnumJobsW(printer, 0, u32::MAX, 1, None, &mut needed, &mut returned) };
        if needed == 0 {
            return Ok(Vec::new());
        }
        if needed > 16 * 1024 * 1024 {
            return Err(format!(
                "print queue response is too large ({needed} bytes)"
            ));
        }
        let word_count = (needed as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buffer = vec![0u64; word_count];
        let byte_len = buffer.len() * std::mem::size_of::<u64>();
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(buffer.as_mut_ptr().cast::<u8>(), byte_len) };
        unsafe {
            EnumJobsW(
                printer,
                0,
                u32::MAX,
                1,
                Some(bytes),
                &mut needed,
                &mut returned,
            )
        }
        .map_err(|error| format!("EnumJobsW: {error}"))?;
        if returned as usize > byte_len / std::mem::size_of::<JOB_INFO_1W>() {
            return Err("Windows returned an invalid print-job count".into());
        }
        let infos = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr().cast::<JOB_INFO_1W>(), returned as usize)
        };
        Ok(infos
            .iter()
            .map(|job| SpoolJob {
                id: job.JobId,
                status: job.Status,
            })
            .collect())
    })();
    let _ = unsafe { ClosePrinter(printer) };
    result
}

fn delete_spool_job(printer_name: &str, job_id: u32) -> Result<(), String> {
    let name = HSTRING::from(printer_name);
    let defaults = PRINTER_DEFAULTSW {
        pDatatype: windows::core::PWSTR::null(),
        pDevMode: std::ptr::null_mut::<DEVMODEW>(),
        DesiredAccess: PRINTER_ACCESS_ADMINISTER,
    };
    let mut printer = PRINTER_HANDLE::default();
    unsafe { OpenPrinterW(&name, &mut printer, Some(&defaults)) }
        .map_err(|error| format!("OpenPrinterW: {error}"))?;
    let deleted = unsafe { SetJobW(printer, job_id, 0, None, JOB_CONTROL_DELETE) }.as_bool();
    let delete_error = if deleted {
        None
    } else {
        Some(unsafe { GetLastError() }.0)
    };
    let close_result = unsafe { ClosePrinter(printer) };
    if let Some(error) = delete_error {
        return Err(format!("SetJobW failed with Windows error {error}"));
    }
    close_result.map_err(|error| format!("ClosePrinter: {error}"))
}

pub(crate) fn resume(app: &tauri::AppHandle) {
    app.manage(PrintServer::default());
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = app.state::<PrintServer>().start(&app, false).await {
            log::warn!("[campus-printer] could not resume Windows queue: {error}");
        }
    });
}

#[tauri::command]
pub async fn install_campus_printer(
    webview: tauri::Webview,
    app: tauri::AppHandle,
    state: tauri::State<'_, Setup>,
    prompt: String,
) -> Result<CampusPrinterSetup, CommandError> {
    crate::require_local_main(&webview)?;
    let _busy = state
        .0
        .try_lock()
        .map_err(|_| "The printer is already being added.".to_string())?;
    let _ = prompt;
    let account = core_ports::core(&app).campus_printer_account().await;
    if !matches!(account, PrinterAccount::Ready { .. }) {
        return Ok(CampusPrinterSetup::Account { account });
    }
    let server = app.state::<PrintServer>();
    let was_running = server
        .0
        .lock()
        .await
        .as_ref()
        .is_some_and(|running| !running.task.is_finished());
    server.start(&app, true).await?;
    let (uri, marker) = {
        let running = server.0.lock().await;
        let endpoint = &running
            .as_ref()
            .ok_or("Printer receiver did not start")?
            .endpoint;
        (endpoint.http_uri(), endpoint.marker())
    };
    let result = tauri::async_runtime::spawn_blocking(move || install_queue(&uri, &marker))
        .await
        .map_err(|e| e.to_string())?;
    match result {
        Ok(true) => {
            server.persist_endpoint(&app).await?;
            Ok(CampusPrinterSetup::Added)
        }
        Ok(false) => {
            if !was_running {
                server.shutdown().await;
            }
            Ok(CampusPrinterSetup::Cancelled)
        }
        Err(error) => {
            log::warn!("[campus-printer] Windows queue installation failed: {error}");
            if was_running {
                // The queue may already reference the live endpoint.
                let _ = server.persist_endpoint(&app).await;
            } else if queue_update_may_have_completed(&error) {
                // Preserve a new endpoint if Windows may have switched the
                // queue before a later configuration step failed.
                let _ = server.persist_endpoint(&app).await;
            } else {
                // A preflight failure (especially pending jobs) must not leave
                // the endpoint moved while Windows still points at the old URL.
                server.shutdown().await;
            }
            Err(error.into())
        }
    }
}

fn queue_update_may_have_completed(error: &str) -> bool {
    error.contains("at remove-previous-queue")
        || error.contains("at add-ipp-queue")
        || error.contains("at configure-queue-access")
        || error.contains("at configure-print-defaults")
        || error.contains("at verify-queue")
        || error.contains("Windows did not confirm printer installation")
}

fn powershell() -> Result<PathBuf, String> {
    Ok(
        PathBuf::from(std::env::var_os("SystemRoot").ok_or("SystemRoot is not set")?)
            .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
    )
}
fn identity() -> Result<(PathBuf, String, String), String> {
    // Capture the interactive caller's SID before UAC; another administrator may
    // supply the elevation credentials, but must not become the queue's owner.
    let shell = powershell()?;
    let sid = Command::new(&shell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
        ])
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| e.to_string())?;
    if !sid.status.success() {
        return Err("Could not resolve the printing user's SID".into());
    }
    let sid = String::from_utf8(sid.stdout).map_err(|e| e.to_string())?;
    let sid = sid.trim();
    if !sid.starts_with("S-1-") || !sid[4..].chars().all(|c| c.is_ascii_digit() || c == '-') {
        return Err("Invalid printing user SID".into());
    }
    let name = format!(
        "IsctCompanion Campus Printer ({:.12})",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(sid.as_bytes()))
    );
    Ok((shell, sid.to_string(), name))
}

fn install_queue(uri: &str, marker: &str) -> Result<bool, String> {
    let (shell, sid, name) = identity()?;
    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let diagnostic =
        std::env::temp_dir().join(format!("isct-campus-printer-{}.log", uuid::Uuid::new_v4()));
    let script = format!(
        r#"$ErrorActionPreference='Stop'
$stage='inspect-queue'
try {{
 $name={name}; $uri={uri}; $marker={marker}; $owner='IsctCompanion:{sid}'
 $existing=Get-Printer -Name $name -Full -ErrorAction SilentlyContinue
 if ($existing -and ($existing.DriverName -ne 'Microsoft IPP Class Driver' -or $existing.Location -ne $owner)) {{ throw 'Printer name belongs to another queue' }}
 # Only migrate this user's old app queues, never arbitrary printers or jobs.
 $stage='inspect-legacy-queues'
 $legacy=@(Get-Printer -Full | Where-Object {{ $_.Name -match '^IsctCompanion Campus Printer - [0-9a-f]{{32}}$' -and $_.DriverName -eq 'Microsoft IPP Class Driver' -and $_.PermissionSDDL -and $_.PermissionSDDL.Contains(';;;{sid})') }})
 $replace=$existing -and $existing.Comment -ne $marker
 $remove=@($legacy)
 if ($replace) {{ $remove+= $existing }}
 $stage='check-pending-jobs'
 foreach ($printer in $remove) {{ if (@(Get-PrintJob -PrinterName $printer.Name -ErrorAction Stop).Count -gt 0) {{ exit 2 }} }}
 $stage='remove-previous-queue'
 foreach ($printer in $remove) {{ Remove-Printer -Name $printer.Name -ErrorAction Stop }}
 $stage='check-receiver'
 Invoke-WebRequest -Uri ('http://127.0.0.1:' + ([uri]$uri).Port + '/') -UseBasicParsing -ErrorAction Stop | Out-Null
 $stage='add-ipp-queue'
 if (-not $existing -or $replace) {{ Add-Printer -Name $name -IppURL $uri -ErrorAction Stop }}
 $stage='configure-queue-access'
 # Packaged print clients such as Photos need use/read access to enumerate this queue.
 # 0x00020008 is PRINTER_READ (READ_CONTROL plus PRINTER_ACCESS_USE); it grants no management rights.
 Set-Printer -Name $name -Location $owner -Comment $marker -Shared $false -KeepPrintedJobs $false -PermissionSDDL 'D:(A;;GA;;;SY)(A;OIIO;GA;;;SY)(A;;GA;;;BA)(A;OIIO;GA;;;BA)(A;;GA;;;{sid})(A;OIIO;GA;;;{sid})(A;OIIO;GA;;;CO)(A;;0x00020008;;;AC)' -ErrorAction Stop
 $stage='configure-print-defaults'
 Set-PrintConfiguration -PrinterName $name -Color $false -DuplexingMode TwoSidedLongEdge -ErrorAction Stop
 $stage='verify-queue'
 $printer=Get-Printer -Name $name -Full -ErrorAction Stop
 if ($printer.DriverName -ne 'Microsoft IPP Class Driver' -or $printer.Comment -ne $marker) {{ throw 'Printer installation could not be verified' }}
 exit 0
}} catch {{
 $details=$_.Exception.Message
 foreach ($secret in @($uri,$marker,$owner)) {{ if ($secret) {{ $details=$details.Replace($secret,'[redacted]') }} }}
 try {{ [System.IO.File]::WriteAllText({diagnostic}, "$stage|$details", [System.Text.Encoding]::UTF8) }} catch {{}}
 $failureCode=49
 switch ($stage) {{
  'inspect-queue' {{ $failureCode=41 }}
  'inspect-legacy-queues' {{ $failureCode=42 }}
  'check-pending-jobs' {{ $failureCode=43 }}
  'remove-previous-queue' {{ $failureCode=44 }}
  'check-receiver' {{ $failureCode=45 }}
  'add-ipp-queue' {{ $failureCode=46 }}
  'configure-queue-access' {{ $failureCode=47 }}
  'configure-print-defaults' {{ $failureCode=48 }}
  'verify-queue' {{ $failureCode=49 }}
 }}
 exit $failureCode
}}"#,
        name = quote(&name),
        uri = quote(uri),
        marker = quote(marker),
        diagnostic = quote(&diagnostic.to_string_lossy())
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let file = HSTRING::from(shell.as_os_str());
    let parameters = HSTRING::from(format!(
        "-NoProfile -NonInteractive -EncodedCommand {encoded}"
    ));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
        if error.code() == ERROR_CANCELLED.to_hresult() {
            return Ok(false);
        }
        return Err(error.to_string());
    }
    let handle = info.hProcess;
    if handle.is_invalid() {
        return Err("The printer installer did not start".into());
    }
    // The installer performs only idempotent queue configuration. A late finish
    // after timeout is harmless; no document is passed to the elevated process.
    let wait = unsafe { WaitForSingleObject(handle, 120_000) };
    let mut code = 1;
    let result = unsafe { GetExitCodeProcess(handle, &mut code) };
    let _ = unsafe { CloseHandle(handle) };
    let details = std::fs::read_to_string(&diagnostic).ok();
    let _ = std::fs::remove_file(&diagnostic);
    if wait.0 == 0 && result.is_ok() && code == 2 {
        return Err("Finish or cancel pending jobs before repairing the printer".into());
    }
    if wait.0 != 0 || result.is_err() || code != 0 {
        if let Some((stage, message)) = details
            .as_deref()
            .and_then(|value| value.trim_start_matches('\u{feff}').split_once('|'))
        {
            return Err(format!("Printer installation failed at {stage}: {message}"));
        }
        let stage = match code {
            41 => Some("inspect-queue"),
            42 => Some("inspect-legacy-queues"),
            43 => Some("check-pending-jobs"),
            44 => Some("remove-previous-queue"),
            45 => Some("check-receiver"),
            46 => Some("add-ipp-queue"),
            47 => Some("configure-queue-access"),
            48 => Some("configure-print-defaults"),
            49 => Some("verify-queue"),
            _ => None,
        };
        if let Some(stage) = stage {
            return Err(format!(
                "Printer installation failed at {stage}; Windows supplied no diagnostic (exit code {code})"
            ));
        }
        return Err("Windows did not confirm printer installation".into());
    }
    Ok(true)
}

async fn bind_receiver(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueState {
    NotInstalled,
    Ready,
    RepairRequired,
    Unavailable,
}
#[derive(Serialize)]
pub struct WindowsPrinterStatus {
    queue: QueueState,
    receiver: bool,
}

#[tauri::command]
pub async fn campus_windows_printer_status(
    webview: tauri::Webview,
    app: tauri::AppHandle,
) -> Result<WindowsPrinterStatus, CommandError> {
    crate::require_local_main(&webview)?;
    let server = app.state::<PrintServer>();
    let resumed = server.start(&app, false).await.is_ok();
    let marker = {
        let running = server.0.lock().await;
        running
            .as_ref()
            .filter(|r| !r.task.is_finished())
            .map(|r| r.endpoint.marker())
    };
    let receiver = marker.is_some();
    let queue = tauri::async_runtime::spawn_blocking(move || probe_queue(marker.as_deref()))
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or(QueueState::Unavailable);
    Ok(WindowsPrinterStatus {
        queue: if resumed {
            queue
        } else {
            QueueState::Unavailable
        },
        receiver,
    })
}

fn probe_queue(marker: Option<&str>) -> Result<QueueState, String> {
    let (shell, sid, name) = identity()?;
    let script = format!(
        r#"$ErrorActionPreference='Stop'
$p=Get-Printer -Name '{name}' -Full -ErrorAction SilentlyContinue
if ($p) {{ $p | Select-Object DriverName,Location,Comment | ConvertTo-Json -Compress }}
else {{
 $old=@(Get-Printer -Full | Where-Object {{ $_.Name -match '^IsctCompanion Campus Printer - [0-9a-f]{{32}}$' -and $_.DriverName -eq 'Microsoft IPP Class Driver' -and $_.PermissionSDDL -and $_.PermissionSDDL.Contains(';;;{sid})') }})
 if ($old.Count -gt 0) {{ '{{}}' }}
}}"#
    );
    let output = Command::new(shell)
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err("Could not inspect Windows printers".into());
    }
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(QueueState::NotInstalled);
    }
    let data: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    Ok(
        if marker.is_some_and(|m| data["Comment"].as_str() == Some(m))
            && data["DriverName"] == "Microsoft IPP Class Driver"
            && data["Location"] == format!("IsctCompanion:{sid}")
        {
            QueueState::Ready
        } else {
            QueueState::RepairRequired
        },
    )
}

#[cfg(test)]
#[path = "tests/campus_printer_ipp_windows.rs"]
mod tests;
