//! `rustinput windows-setup install [--no-service]` and
//! `rustinput windows-setup uninstall`: what the installer
//! (`build-aux/windows/rustinput.iss`) asks of RustInput itself, as
//! administrator, once its files are in place or before they go.
//!
//! - the Windows service "RustInput" (see [`crate::windows_supervisor`]),
//!   which starts with Windows and which Windows starts again when it fails;
//! - the rules of the Windows firewall that let the other computers reach
//!   RustInput, for the networks the rules Windows made for RustInput before
//!   allowed (a user who allowed public networks keeps them).
//!
//! It logs into `%SystemRoot%\Logs\RustInput\setup.log` (see
//! [`crate::windows_log`]) and ends with [`EXIT_OK`], [`EXIT_USAGE`],
//! [`EXIT_SERVICE`] or [`EXIT_FIREWALL`], which the installer shows.

use std::{
    ffi::OsString,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use rustinput_ipc::WINDOWS_SERVICE;
use windows_service::{
    service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
        ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType,
        ServiceState, ServiceType,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

/// the first argument
pub(crate) const ARGUMENT: &str = "windows-setup";
/// done
pub(crate) const EXIT_OK: i32 = 0;
/// not `install` or `uninstall`
pub(crate) const EXIT_USAGE: i32 = 2;
/// the service could not be installed, started or removed
pub(crate) const EXIT_SERVICE: i32 = 3;
/// the rules of the firewall could not be set or removed
pub(crate) const EXIT_FIREWALL: i32 = 4;

/// the argument Windows starts the service with (see windows_supervisor.rs)
const SERVICE_ARGUMENT: &str = "windows-service";
/// how long the service has to start or to stop
const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);

/// whether this process was started for the installer
pub(crate) fn requested() -> bool {
    std::env::args().nth(1).as_deref() == Some(ARGUMENT)
}

/// Does what the arguments ask and returns the exit code.
pub(crate) fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            log::error!("where is this program? {e}");
            return EXIT_SERVICE;
        }
    };
    log::info!(
        "RustInput {} {ARGUMENT} {}",
        env!("CARGO_PKG_VERSION"),
        args.join(" ")
    );
    match args.first().map(String::as_str) {
        Some("install") => install(&exe, !args.iter().any(|a| a == "--no-service")),
        Some("uninstall") => uninstall(),
        _ => {
            log::error!("usage: rustinput {ARGUMENT} install [--no-service] | uninstall");
            EXIT_USAGE
        }
    }
}

fn install(exe: &Path, with_service: bool) -> i32 {
    let mut code = EXIT_OK;
    // the firewall first: the daemon the service starts listens at once
    match firewall::install(exe) {
        Ok(profiles) => log::info!("firewall: the other computers reach {exe:?} ({profiles})"),
        Err(e) => {
            log::error!("could not set the rules of the firewall: {e}");
            code = EXIT_FIREWALL;
        }
    }
    let service = if with_service {
        install_service(exe)
    } else {
        remove_service()
    };
    if let Err(e) = service {
        log::error!("the service: {e}");
        code = EXIT_SERVICE;
    }
    code
}

fn uninstall() -> i32 {
    let mut code = EXIT_OK;
    if let Err(e) = remove_service() {
        log::error!("could not remove the service: {e}");
        code = EXIT_SERVICE;
    }
    remove_registry();
    match firewall::remove() {
        Ok(0) => {}
        Ok(n) => log::info!("firewall: removed {n} rules"),
        Err(e) => {
            log::error!("could not remove the rules of the firewall: {e}");
            code = EXIT_FIREWALL;
        }
    }
    code
}

fn portuguese() -> bool {
    // the primary language id of Portuguese
    let language = unsafe { GetUserDefaultUILanguage() };
    language & 0x3ff == 0x16
}

/// Installs the service, or brings it up to date, and starts it.
fn install_service(exe: &Path) -> windows_service::Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let info = ServiceInfo {
        name: OsString::from(WINDOWS_SERVICE),
        display_name: OsString::from("RustInput"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.to_path_buf(),
        launch_arguments: vec![OsString::from(SERVICE_ARGUMENT)],
        dependencies: vec![],
        // LocalSystem
        account_name: None,
        account_password: None,
    };
    // START also for the failure actions that restart it
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::CHANGE_CONFIG;
    let service = match manager.open_service(WINDOWS_SERVICE, access) {
        Ok(service) => {
            service.change_config(&info)?;
            log::info!("service: brought up to date");
            service
        }
        Err(_) => {
            let service = manager.create_service(&info, access)?;
            log::info!("service: installed");
            service
        }
    };
    let description = if portuguese() {
        "Mantém o RustInput rodando na sessão de quem está na tela, com direitos de \
         administrador, para alcançar também os programas abertos como administrador."
    } else {
        "Keeps RustInput running in the session of the user at the screen, with \
         administrator rights, so it reaches programs run as administrator too."
    };
    service.set_description(description)?;
    // as update-rustinput.ps1 did: again after 5 s, 10 s, then every minute,
    // counted afresh after a day
    let restart = |seconds| ServiceAction {
        action_type: ServiceActionType::Restart,
        delay: Duration::from_secs(seconds),
    };
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart(5), restart(10), restart(60)]),
    })?;
    // also when it stops with an error of its own, not only when it crashes
    service.set_failure_actions_on_non_crash_failures(true)?;

    let state = service.query_status()?.current_state;
    if state == ServiceState::Stopped {
        service.start::<&str>(&[])?;
    }
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        let state = service.query_status()?.current_state;
        if state == ServiceState::Running {
            log::info!("service: running");
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(windows_service::Error::Winapi(std::io::Error::other(
                format!("it did not start ({state:?} after {SERVICE_TIMEOUT:?})"),
            )));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Removes what the service remembers (`HKLM\SOFTWARE\RustInput`, see
/// `windows_supervisor`), if anything.
fn remove_registry() {
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::{HKEY_LOCAL_MACHINE, RegDeleteTreeW},
    };
    let key: Vec<u16> = crate::windows_supervisor::KEY
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    match unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, key.as_ptr()) } {
        ERROR_SUCCESS => log::info!("removed HKLM\\{}", crate::windows_supervisor::KEY),
        ERROR_FILE_NOT_FOUND => {}
        e => log::warn!(
            "could not remove HKLM\\{}: {}",
            crate::windows_supervisor::KEY,
            std::io::Error::from_raw_os_error(e as i32)
        ),
    }
}

/// Stops and removes the service, if it is installed.
fn remove_service() -> windows_service::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let Ok(service) = manager.open_service(WINDOWS_SERVICE, access) else {
        return Ok(());
    };
    if service.query_status()?.current_state != ServiceState::Stopped {
        // fails when it is stopping already
        let _ = service.stop();
        let deadline = Instant::now() + SERVICE_TIMEOUT;
        while service.query_status()?.current_state != ServiceState::Stopped {
            if Instant::now() > deadline {
                log::warn!("service: did not stop in {SERVICE_TIMEOUT:?}, removed anyway");
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
    service.delete()?;
    drop(service);
    // Windows removes it once no handle to it is left
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    while manager
        .open_service(WINDOWS_SERVICE, ServiceAccess::QUERY_STATUS)
        .is_ok()
    {
        if Instant::now() > deadline {
            log::warn!("service: still there, Windows removes it at the next start");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    log::info!("service: removed");
    Ok(())
}

mod firewall {
    //! The rules of the Windows firewall, through its COM interface: one for
    //! UDP (keyboard, mouse, clipboard) and one for TCP (files, pairing
    //! codes), in the group "RustInput", inbound, for the executable only.

    use std::path::Path;

    use windows::{
        Win32::{
            Foundation::{VARIANT_FALSE, VARIANT_TRUE},
            NetworkManagement::WindowsFirewall::{
                INetFwPolicy2, INetFwRule, INetFwRules, NET_FW_ACTION_ALLOW,
                NET_FW_IP_PROTOCOL_TCP, NET_FW_IP_PROTOCOL_UDP, NET_FW_PROFILE2_ALL,
                NET_FW_PROFILE2_DOMAIN, NET_FW_PROFILE2_PRIVATE, NET_FW_PROFILE2_PUBLIC,
                NET_FW_RULE_DIR_IN, NetFwPolicy2, NetFwRule,
            },
            System::{
                Com::{
                    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance,
                    CoInitializeEx, CoUninitialize, IDispatch,
                },
                Ole::IEnumVARIANT,
                Variant::{VARIANT, VT_DISPATCH, VariantClear},
            },
        },
        core::{BSTR, Interface, Result},
    };

    /// the group of the rules, as update-rustinput.ps1 made them
    const GROUP: &str = "RustInput";
    /// removing one rule after another: more of ours is a mistake
    const MOST_RULES: usize = 64;

    /// COM for this thread while it lives: every interface must be released
    /// before it ends
    struct Com;

    impl Com {
        fn init() -> Result<Self> {
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()?;
            Ok(Com)
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    struct Found {
        name: BSTR,
        ours: bool,
        /// an enabled inbound rule that allows rustinput.exe
        allows_rustinput: bool,
        profiles: i32,
    }

    fn rules() -> Result<INetFwRules> {
        let policy: INetFwPolicy2 =
            unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }?;
        unsafe { policy.Rules() }
    }

    /// every rule of the firewall, read as far as RustInput cares
    fn find(rules: &INetFwRules) -> Result<Vec<Found>> {
        let enumerator: IEnumVARIANT = unsafe { rules._NewEnum() }?.cast()?;
        let mut found = Vec::new();
        loop {
            let mut items = [VARIANT::default()];
            let mut fetched = 0u32;
            let more = unsafe { enumerator.Next(&mut items, &mut fetched) };
            if more.is_err() || fetched == 0 {
                break;
            }
            let item = &mut items[0];
            let dispatch: Option<IDispatch> = unsafe {
                if item.Anonymous.Anonymous.vt == VT_DISPATCH {
                    (*item.Anonymous.Anonymous.Anonymous.pdispVal).clone()
                } else {
                    None
                }
            };
            let _ = unsafe { VariantClear(item) };
            let Some(rule) = dispatch.and_then(|d| d.cast::<INetFwRule>().ok()) else {
                continue;
            };
            let read = || -> Result<Found> {
                unsafe {
                    let application = rule.ApplicationName().unwrap_or_default().to_string();
                    let allows = rule.Direction()? == NET_FW_RULE_DIR_IN
                        && rule.Action()? == NET_FW_ACTION_ALLOW
                        && rule.Enabled()? != VARIANT_FALSE
                        && application.to_lowercase().ends_with("\\rustinput.exe");
                    Ok(Found {
                        name: rule.Name()?,
                        ours: rule.Grouping().unwrap_or_default() == GROUP,
                        allows_rustinput: allows,
                        profiles: rule.Profiles()?,
                    })
                }
            };
            if let Ok(rule) = read() {
                found.push(rule);
            }
        }
        Ok(found)
    }

    /// Removes the rules of the group, one name after another: rules may
    /// share a name, and Remove takes one of them at a time.
    fn remove_ours(rules: &INetFwRules) -> Result<usize> {
        let mut removed = 0;
        while removed < MOST_RULES {
            let Some(rule) = find(rules)?.into_iter().find(|r| r.ours) else {
                break;
            };
            unsafe { rules.Remove(&rule.name) }?;
            removed += 1;
        }
        Ok(removed)
    }

    fn profile_names(profiles: i32) -> String {
        if profiles == NET_FW_PROFILE2_ALL.0 {
            return "all networks".into();
        }
        [
            (NET_FW_PROFILE2_DOMAIN.0, "domain"),
            (NET_FW_PROFILE2_PRIVATE.0, "private"),
            (NET_FW_PROFILE2_PUBLIC.0, "public"),
        ]
        .iter()
        .filter(|(bit, _)| profiles & bit != 0)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(", ")
    }

    /// Puts the rules of the group in place for `exe` and returns the
    /// networks they allow.
    pub(super) fn install(exe: &Path) -> Result<String> {
        let _com = Com::init()?;
        let rules = rules()?;
        let found = find(&rules)?;
        // the networks of the rules of the group, or else of those Windows
        // made when it asked about rustinput.exe, or else domain and private
        let of = |ours: bool| {
            found
                .iter()
                .filter(|r| r.ours == ours && r.allows_rustinput)
                .fold(0, |profiles, r| profiles | r.profiles)
        };
        let mut profiles = of(true);
        if profiles == 0 {
            profiles = of(false);
        }
        if profiles == 0 {
            profiles = NET_FW_PROFILE2_DOMAIN.0 | NET_FW_PROFILE2_PRIVATE.0;
        }
        let removed = remove_ours(&rules)?;
        if removed > 0 {
            log::info!("firewall: replacing {removed} rules");
        }
        let application = BSTR::from(exe.display().to_string());
        for (protocol, name) in [
            (NET_FW_IP_PROTOCOL_UDP, "RustInput (UDP)"),
            (NET_FW_IP_PROTOCOL_TCP, "RustInput (TCP)"),
        ] {
            unsafe {
                let rule: INetFwRule = CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER)?;
                rule.SetName(&BSTR::from(name))?;
                rule.SetDescription(&BSTR::from(
                    "Lets the other computers of RustInput reach it (installed by its setup)",
                ))?;
                rule.SetGrouping(&BSTR::from(GROUP))?;
                rule.SetApplicationName(&application)?;
                rule.SetProtocol(protocol.0)?;
                rule.SetDirection(NET_FW_RULE_DIR_IN)?;
                rule.SetAction(NET_FW_ACTION_ALLOW)?;
                rule.SetProfiles(profiles)?;
                rule.SetEnabled(VARIANT_TRUE)?;
                rules.Add(&rule)?;
            }
        }
        Ok(profile_names(profiles))
    }

    /// Removes the rules of the group; returns how many there were.
    pub(super) fn remove() -> Result<usize> {
        let _com = Com::init()?;
        // a local, released before COM ends: a temporary of the last
        // expression would outlive `_com`, and releasing it then crashes
        let rules = rules()?;
        let removed = remove_ours(&rules)?;
        Ok(removed)
    }
}
