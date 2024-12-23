use std::sync::Arc;

use peer::peer::{Peer, PeerOptions};
use webrtc::{
  api::{
    interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
  },
  ice_transport::ice_server::RTCIceServer,
  interceptor::registry::Registry,
  peer_connection::configuration::RTCConfiguration,
};

#[tokio::main]
async fn main() -> Result<(), webrtc::Error> {
  let mut m = MediaEngine::default();
  let registry = register_default_interceptors(Registry::new(), &mut m)?;

  let api = Arc::new(
    APIBuilder::new()
      .with_media_engine(m)
      .with_interceptor_registry(registry)
      .build(),
  );

  let options = PeerOptions {
    connection_config: Some(RTCConfiguration {
      ice_servers: vec![RTCIceServer {
        ..Default::default()
      }],
      ..Default::default()
    }),
    ..Default::default()
  };

  let peer1 = Peer::new(
    api.clone(),
    PeerOptions {
      id: Some("peer1".to_string()),
      ..options.clone()
    },
  );
  let peer2 = Peer::new(
    api,
    PeerOptions {
      id: Some("peer2".to_string()),
      ..options
    },
  );

  let on_signal_peer2 = peer2.clone();
  peer1
    .on_signal(Box::new(move |singal| {
      let pinned_peer2 = on_signal_peer2.clone();
      Box::pin(async move {
        pinned_peer2
          .signal(singal)
          .await
          .expect("failed to signal peer2");
      })
    }))
    .await;

  let on_signal_peer1 = peer1.clone();
  peer2
    .on_signal(Box::new(move |singal| {
      let pinned_peer1 = on_signal_peer1.clone();
      Box::pin(async move {
        pinned_peer1
          .signal(singal)
          .await
          .expect("failed to signal peer1");
      })
    }))
    .await;

  let (connect_sender, mut connect_receiver) = tokio::sync::mpsc::channel::<()>(1);
  peer2
    .on_connect(Box::new(move || {
      let pinned_connect_sender = connect_sender.clone();
      Box::pin(async move {
        pinned_connect_sender
          .send(())
          .await
          .expect("failed to send connect");
      })
    }))
    .await;

  let (message_sender, mut message_receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
  peer1
    .on_data(Box::new(move |data| {
      let pinned_message_sender = message_sender.clone();
      Box::pin(async move {
        pinned_message_sender
          .send(data)
          .await
          .expect("failed to send connect");
      })
    }))
    .await;

  peer1.init().await?;

  let _ = connect_receiver.recv().await;
  if let Some(data_channel) = peer2.get_data_channel() {
    data_channel.send_text("Hello, world!").await?;
  }

  let data = message_receiver
    .recv()
    .await
    .expect("failed to receive message from peer2");

  assert_eq!(String::from_utf8_lossy(data.as_ref()), "Hello, world!");

  peer1.close().await?;
  peer2.close().await?;

  Ok(())
}
