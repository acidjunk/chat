use axum::extract::WebSocketUpgrade;
use axum::response::IntoResponse;
use axum::{extract::ws, extract::State, routing::get, Router};
use axum_chat::hub::{Hub, HubOptions};
use axum_chat::proto::InputParcel;
use futures::{StreamExt, TryStreamExt};
use log::{error, info};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;

const MAX_FRAME_SIZE: usize = 1 << 16;

struct AppState {
    hub: Arc<Hub>,
    tx: UnboundedSender<InputParcel>,
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let (input_sender, input_receiver) = mpsc::unbounded_channel::<InputParcel>();
    let rev_stream = tokio_stream::wrappers::UnboundedReceiverStream::new(input_receiver);

    let hub: Arc<Hub> = Arc::new(Hub::new(HubOptions {
        alive_interval: Some(Duration::from_secs(5)),
    }));
    let app_state = Arc::new(AppState {
        hub: hub.clone(),
        tx: input_sender,
    });
    // ||

    let shutdown = || async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install CTRL+C signal handler");
    };

    let routes = Router::new()
        .route("/feed", axum::routing::get(websocket_handler))
        .with_state(app_state);
    let srv = axum::Server::bind(&SocketAddr::from(([127, 0, 0, 1], 8080)))
        .serve(routes.into_make_service())
        .with_graceful_shutdown(shutdown());

    let running_hub = hub.run(rev_stream);

    tokio::select! {
        _ = running_hub => {},
        _ = srv => {},
    }
}

async fn websocket_handler(
    ws: ws::WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // add code here
    ws.max_frame_size(MAX_FRAME_SIZE)
        .on_upgrade(move |socket| async move {
            tokio::spawn(process_client(socket, state));
        })
}

async fn process_client(web_socket: ws::WebSocket, state: Arc<AppState>) {
    let output_receiver = state.hub.subscribe();
    let cli_out_rec = tokio_stream::wrappers::BroadcastStream::new(output_receiver);
    let (ws_sink, ws_stream) = web_socket.split();
    let client = axum_chat::client::Client::new();

    info!("Client {} connected", client.id);

    let reading = client
        .read_input(ws_stream)
        .try_for_each(|input_parcel| async {
            state.tx.send(input_parcel).unwrap();
            Ok(())
        });

    let (tx, rx) = mpsc::unbounded_channel();
    let clien_rx = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
    tokio::spawn(clien_rx.forward(ws_sink));
    let writing = client
        .write_output(cli_out_rec)
        .try_for_each(|message| async {
            tx.send(Ok(message)).unwrap();
            Ok(())
        });

    if let Err(err) = tokio::select! {
        result = reading => result,
        result = writing => result,
    } {
        error!("Client connection error: {}", err);
    }

    state.hub.on_disconnect(client.id).await;
    info!("Client {} disconnected", client.id);
}
