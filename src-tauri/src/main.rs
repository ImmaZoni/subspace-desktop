#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
mod windows;

mod menu;
mod node;

use anyhow::Result;
use log::{debug, error, info};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use subspace_core_primitives::{PublicKey, PIECE_SIZE};
use subspace_farmer::{
    Commitments, FarmerData, Farming, Identity, ObjectMappings, Plot, Plotting, RpcClient, WsRpc,
};
use subspace_solving::SubspaceCodec;
use tauri::SystemTrayEvent;
use tauri::{
    api::{self},
    Env, Manager, RunEvent,
};
use tokio::runtime::Handle;

static PLOTTED_PIECES: AtomicUsize = AtomicUsize::new(0);
const BEST_BLOCK_NUMBER_CHECK_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Serialize)]
struct DiskStats {
    free_bytes: u64,
    total_bytes: u64,
}

#[tauri::command]
fn plot_progress_tracker() -> usize {
    PLOTTED_PIECES.load(Ordering::Relaxed)
}

#[tauri::command]
async fn farming(path: String, reward_address: String, plot_size: u64) {
    match reward_address.len() {
        0 => {
            farm(path.into(), "ws://127.0.0.1:9944", None, plot_size)
                .await
                .unwrap();
        }
        _ => {
            if let Ok(address) = parse_reward_address(&reward_address) {
                farm(path.into(), "ws://127.0.0.1:9944", Some(address), plot_size)
                    .await
                    .unwrap();
            } else {
                error!("Reward address could not be parsed!");
            }
        }
    }
}

#[tauri::command]
async fn start_node(path: String) -> [u8; 32] {
    init_node(path.into()).await.unwrap()
}

#[tauri::command]
fn get_disk_stats(dir: String) -> DiskStats {
    debug!("{}", dir);
    let free: u64 = fs2::available_space(&dir).expect("error");
    let total: u64 = fs2::total_space(&dir).expect("error");

    DiskStats {
        free_bytes: free,
        total_bytes: total,
    }
}

#[tauri::command]
fn get_this_binary() -> PathBuf {
    let bin = api::process::current_binary(&Env::default());
    bin.unwrap()
}

#[tokio::main]
async fn main() -> Result<()> {
    let app = tauri::Builder::default()
        .menu(menu::get_menu())
        .system_tray(menu::get_tray_menu())
        .on_system_tray_event(|app, event| {
            if let SystemTrayEvent::MenuItemClick { id, .. } = event {
                let item_handle = app.tray_handle().get_item(&id);
                match id.as_str() {
                    "quit" => {
                        std::process::exit(0); // hide window
                    }
                    "toggle_visibility" => {
                        let window = app.get_window("main").unwrap();
                        if window.is_visible().unwrap() {
                            window.hide().unwrap();
                            //#[cfg(target_os = "macos")]
                            // app.set_activation_policy(tauri::ActivationPolicy::Accessory); // TODO This should hide the main taskbar icon when the window is hidden, however there is a borrow error

                            item_handle.set_title("Show").unwrap();
                        } else {
                            window.show().unwrap();
                            //#[cfg(target_os = "macos")]
                            // app.set_activation_policy(tauri::ActivationPolicy::Regular); // TODO This should show the main taskbar icon when the window is visible, however there is a borrow error
                            item_handle.set_title("Hide").unwrap();
                        }
                    }
                    _ => {}
                }
            }
        })
        .invoke_handler(
            #[cfg(not(target_os = "windows"))]
            tauri::generate_handler![
                get_disk_stats,
                get_this_binary,
                farming,
                plot_progress_tracker,
                start_node
            ],
            #[cfg(target_os = "windows")]
            tauri::generate_handler![
                windows::winreg_get,
                windows::winreg_set,
                windows::winreg_delete,
                get_this_binary,
                get_disk_stats,
                farming,
                plot_progress_tracker,
                start_node
            ],
        )
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    app.run(|app_handle, e| match e {
        RunEvent::CloseRequested { label, api, .. } => {
            let app_handle = app_handle.clone();
            let window = app_handle.get_window(&label).unwrap();
            // use the exposed close api, and prevent the event loop to close
            api.prevent_close();
            // hide the window
            window.hide().unwrap();
            // TODO This should hide the main taskbar icon when the window is closed on macos, however there is a borrow error
            // #[cfg(target_os = "macos")]
            // app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let tray_handle = app_handle.tray_handle();
            let item_handle = tray_handle.get_item("toggle_visibility");
            item_handle.set_title("Show").unwrap(); // update the tray menu title to reflect the hidden state of the window
        }
        RunEvent::ExitRequested { api, .. } => {
            api.prevent_exit();
        }
        _ => {}
    });

    Ok(())
}

async fn init_node(base_directory: PathBuf) -> Result<[u8; 32]> {
    let identity = Identity::open_or_create(&base_directory)?;
    let public_key = identity.public_key().to_bytes();

    let chain_spec =
        sc_service::GenericChainSpec::<subspace_runtime::GenesisConfig>::from_json_bytes(
            include_bytes!("../chain-spec.json").as_ref(),
        )
        .map_err(anyhow::Error::msg)?;

    let full_client_fut = tokio::task::spawn_blocking(move || {
        Handle::current().block_on(node::create_full_client(chain_spec, base_directory))
    });
    let mut full_client = full_client_fut.await??;

    // TODO: Make this interruptable if needed
    tokio::spawn(async move {
        if let Err(error) = full_client.task_manager.future().await {
            error!("Task manager exited with error: {error}");
        } else {
            error!("Task manager exited without error");
        }
    });

    Ok(public_key)
}

/// Start farming by using plot in specified path and connecting to WebSocket server at specified
/// address.
async fn farm(
    base_directory: PathBuf,
    node_rpc_url: &str,
    reward_address: Option<PublicKey>,
    plot_size: u64,
) -> Result<()> {
    let identity = Identity::open_or_create(&base_directory)?;
    let address = identity.public_key().to_bytes().into();

    let reward_address = reward_address.unwrap_or_else(|| identity.public_key().to_bytes().into());

    info!("Connecting to node at {}", node_rpc_url);
    let client = WsRpc::new(node_rpc_url).await?;

    let farmer_metadata = client
        .farmer_metadata()
        .await
        .map_err(|error| anyhow::Error::msg(error.to_string()))?;

    // TODO: This doesn't account for the fact that node can
    // have a completely different history to what farmer expects
    info!("Opening plot");
    let plot_fut = tokio::task::spawn_blocking({
        let base_directory = base_directory.clone();
        let plot_size = plot_size / PIECE_SIZE as u64;

        // TODO: Piece count should account for database overhead of various additional databases
        move || Plot::open_or_create(&base_directory, address, Some(plot_size))
    });
    let plot = plot_fut.await.unwrap()?;

    // Keep track of the plotting for Desktop App
    plot.on_progress_change(Arc::new(|plotted_pieces| {
        PLOTTED_PIECES.fetch_add(
            plotted_pieces.plotted_piece_count / PIECE_SIZE,
            Ordering::SeqCst,
        );
        debug!(
            "Plotted pieces so far: {}",
            PLOTTED_PIECES.load(Ordering::Relaxed)
        );
    }))
    .detach();

    info!("Opening commitments");
    let commitments_fut = tokio::task::spawn_blocking({
        let path = base_directory.join("commitments");

        move || Commitments::new(path)
    });
    let commitments = commitments_fut.await.unwrap()?;

    info!("Opening object mapping");
    let object_mappings = tokio::task::spawn_blocking({
        let base_directory = base_directory.clone();

        move || ObjectMappings::open_or_create(&base_directory)
    })
    .await??;

    let subspace_codec = SubspaceCodec::new(identity.public_key());

    // start the farming task
    let farming_instance = Farming::start(
        plot.clone(),
        commitments.clone(),
        client.clone(),
        identity,
        reward_address,
    );

    let farmer_data = FarmerData::new(plot, commitments, object_mappings, farmer_metadata);

    // start the background plotting
    let plotting_instance = Plotting::start(
        farmer_data,
        client,
        subspace_codec,
        BEST_BLOCK_NUMBER_CHECK_INTERVAL,
    );

    // wait for the farming and plotting in the background
    tokio::spawn(async {
        tokio::select! {
            res = plotting_instance.wait() => if let Err(error) = res {
                error!("Plotting created the error: {error}");
            },
            res = farming_instance.wait() => if let Err(error) = res {
                error!("Farming created the error: {error}");
            },
        }
    });

    Ok(())
}

fn parse_reward_address(s: &str) -> Result<PublicKey, sp_core::crypto::PublicError> {
    s.parse::<sp_core::sr25519::Public>()
        .map(|key| PublicKey::from(key.0))
}
