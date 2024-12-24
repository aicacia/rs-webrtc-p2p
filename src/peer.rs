use std::{
  future::Future,
  pin::Pin,
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
};

use serde::{Deserialize, Serialize};
use tokio::{runtime::Handle, sync::Mutex, task::block_in_place};
use uuid::Uuid;
use webrtc::{
  api::API,
  data_channel::{
    data_channel_init::RTCDataChannelInit, data_channel_message::DataChannelMessage, RTCDataChannel,
  },
  ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
  peer_connection::{
    configuration::RTCConfiguration,
    offer_answer_options::{RTCAnswerOptions, RTCOfferOptions},
    peer_connection_state::RTCPeerConnectionState,
    sdp::{sdp_type::RTCSdpType, session_description::RTCSessionDescription},
    RTCPeerConnection,
  },
  rtp_transceiver::{rtp_receiver::RTCRtpReceiver, RTCRtpTransceiver},
  track::track_remote::TrackRemote,
};

use crate::atomic_option::AtomicOption;

#[derive(Clone, Default)]
pub struct PeerOptions {
  pub id: Option<String>,
  pub max_channel_message_size: Option<usize>,
  pub data_channel_name: Option<String>,
  pub event_channel_size: Option<usize>,
  pub connection_config: Option<RTCConfiguration>,
  pub offer_config: Option<RTCOfferOptions>,
  pub answer_config: Option<RTCAnswerOptions>,
  pub data_channel_config: Option<RTCDataChannelInit>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum SignalMessage {
  #[serde(rename = "renegotiate")]
  Renegotiate,
  #[serde(rename = "candidate")]
  Candidate(RTCIceCandidateInit),
  #[serde(untagged)]
  Answer(RTCSessionDescription),
  #[serde(untagged)]
  Offer(RTCSessionDescription),
  #[serde(untagged)]
  Pranswer(RTCSessionDescription),
  #[serde(untagged)]
  Rollback(RTCSessionDescription),
}

pub type OnSignal = Box<
  dyn (FnMut(SignalMessage) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync,
>;
pub type OnData =
  Box<dyn (FnMut(Vec<u8>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;
pub type OnConnect =
  Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;
pub type OnClose =
  Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;
pub type OnNegotiated =
  Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

#[derive(Clone)]
pub struct Peer {
  inner: Arc<PeerInner>,
}

unsafe impl Send for Peer {}
unsafe impl Sync for Peer {}

pub struct PeerInner {
  id: String,
  api: Arc<API>,
  initiator: Arc<AtomicBool>,
  connection_config: RTCConfiguration,
  connection: AtomicOption<RTCPeerConnection>,
  offer_config: Option<RTCOfferOptions>,
  answer_config: Option<RTCAnswerOptions>,
  data_channel_name: String,
  data_channel: AtomicOption<Arc<RTCDataChannel>>,
  data_channel_config: Option<RTCDataChannelInit>,
  pending_candidates: Mutex<Vec<RTCIceCandidateInit>>,
  on_signal: Arc<Mutex<Option<OnSignal>>>,
  on_data: Arc<Mutex<Option<OnData>>>,
  on_connect: Arc<Mutex<Option<OnConnect>>>,
  on_close: Arc<Mutex<Option<OnClose>>>,
  on_negotiated: Arc<Mutex<Option<OnNegotiated>>>,
}

impl Peer {
  pub fn new(api: Arc<API>, options: PeerOptions) -> Self {
    Self {
      inner: Arc::new(PeerInner {
        id: options.id.unwrap_or_else(|| Uuid::new_v4().to_string()),
        api,
        initiator: Arc::new(AtomicBool::new(false)),
        data_channel_name: options
          .data_channel_name
          .unwrap_or_else(|| Uuid::new_v4().to_string()),
        connection: AtomicOption::none(),
        connection_config: options.connection_config.unwrap_or_default(),
        offer_config: options.offer_config,
        answer_config: options.answer_config,
        data_channel_config: options.data_channel_config,
        data_channel: AtomicOption::none(),
        pending_candidates: Mutex::new(Vec::new()),
        on_signal: Arc::new(Mutex::new(None)),
        on_data: Arc::new(Mutex::new(None)),
        on_connect: Arc::new(Mutex::new(None)),
        on_close: Arc::new(Mutex::new(None)),
        on_negotiated: Arc::new(Mutex::new(None)),
      }),
    }
  }

  pub fn get_id(&self) -> &str {
    &self.inner.id
  }

  pub fn get_data_channel(&self) -> Option<Arc<RTCDataChannel>> {
    self
      .inner
      .data_channel
      .as_ref(Ordering::Relaxed)
      .map(Clone::clone)
  }

  pub fn get_connection(&self) -> Option<&RTCPeerConnection> {
    self.inner.connection.as_ref(Ordering::Relaxed)
  }

  pub async fn init(&self) -> Result<(), webrtc::Error> {
    self.inner.initiator.store(true, Ordering::SeqCst);
    self.create_peer().await
  }

  pub fn on_signal(&self, callback: OnSignal) {
    block_in_place(|| {
      Handle::current()
        .block_on(self.inner.on_signal.lock())
        .replace(callback)
    });
  }

  pub fn on_data(&self, callback: OnData) {
    block_in_place(|| {
      Handle::current()
        .block_on(self.inner.on_data.lock())
        .replace(callback)
    });
  }

  pub fn on_connect(&self, callback: OnConnect) {
    block_in_place(|| {
      Handle::current()
        .block_on(self.inner.on_connect.lock())
        .replace(callback)
    });
  }

  pub fn on_close(&self, callback: OnClose) {
    block_in_place(|| {
      Handle::current()
        .block_on(self.inner.on_close.lock())
        .replace(callback)
    });
  }

  pub fn on_negotiated(&self, callback: OnNegotiated) {
    block_in_place(|| {
      Handle::current()
        .block_on(self.inner.on_negotiated.lock())
        .replace(callback)
    });
  }

  async fn create_peer(&self) -> Result<(), webrtc::Error> {
    let api = self.inner.api.clone();
    let connection =
      self
        .inner
        .connection
        .load_or_store_with(Ordering::SeqCst, Ordering::SeqCst, || {
          block_in_place(|| {
            Handle::current()
              .block_on(api.new_peer_connection(self.inner.connection_config.clone()))
              .expect("failed to create peer connection")
          })
        });

    let on_negotiation_needed_peer = self.clone();
    connection.on_negotiation_needed(Box::new(move || {
      let pinned_peer = on_negotiation_needed_peer.clone();
      Box::pin(async move {
        pinned_peer.on_negotiation_needed().await;
      })
    }));
    let on_peer_connection_state_change_peer = self.clone();
    connection.on_peer_connection_state_change(Box::new(move |connection_state| {
      let pinned_peer = on_peer_connection_state_change_peer.clone();
      Box::pin(async move {
        pinned_peer
          .on_peer_connection_state_change(connection_state)
          .await;
      })
    }));
    let on_ice_candidate_peer = self.clone();
    connection.on_ice_candidate(Box::new(move |candidate| {
      let pinned_peer = on_ice_candidate_peer.clone();
      Box::pin(async move {
        pinned_peer.on_ice_candidate(candidate).await;
      })
    }));
    let on_track_peer = self.clone();
    connection.on_track(Box::new(move |track, receiver, transceiver| {
      let pinned_peer = on_track_peer.clone();
      Box::pin(async move {
        pinned_peer.on_track(track, receiver, transceiver).await;
      })
    }));

    if self.inner.initiator.load(Ordering::Relaxed) {
      self.on_data_channel(
        connection
          .create_data_channel(
            &self.inner.data_channel_name,
            self.inner.data_channel_config.clone(),
          )
          .await?,
      );
    } else {
      let peer = self.clone();
      connection.on_data_channel(Box::new(move |data_channel| {
        peer.on_data_channel(data_channel);
        Box::pin(async move {})
      }));
    }

    Ok(())
  }

  pub async fn close(&self) -> Result<(), webrtc::Error> {
    self.internal_close(true).await
  }

  async fn on_negotiation_needed(&self) {
    match self.negotiate().await {
      Ok(_) => {}
      Err(error) => {
        eprintln!("error negotiating: {}", error)
      }
    }
  }

  async fn on_peer_connection_state_change(&self, connection_state: RTCPeerConnectionState) {
    match connection_state {
      RTCPeerConnectionState::Closed
      | RTCPeerConnectionState::Failed
      | RTCPeerConnectionState::Disconnected => match self.internal_close(true).await {
        Ok(_) => {}
        Err(error) => {
          eprintln!("error peer connection change: {}", error)
        }
      },
      _state => {}
    }
  }
  async fn on_ice_candidate(&self, candidate: Option<RTCIceCandidate>) {
    if let Some(candidate) = candidate {
      let candidate = match candidate.to_json() {
        Ok(candidate) => candidate,
        Err(error) => {
          eprintln!("error ice candidate: {}", error);
          return;
        }
      };
      self
        .internal_on_signal(SignalMessage::Candidate(candidate))
        .await;
    }
  }
  async fn on_track(
    &self,
    track: Arc<TrackRemote>,
    receiver: Arc<RTCRtpReceiver>,
    transceiver: Arc<RTCRtpTransceiver>,
  ) {
    println!(
      "{}: track: {:?} {:?} {:?}",
      self.get_id(),
      track,
      receiver,
      transceiver
    );
  }

  fn on_data_channel(&self, data_channel: Arc<RTCDataChannel>) {
    let on_open_peer = self.clone();
    data_channel.on_open(Box::new(move || {
      let pinned_peer = on_open_peer.clone();
      Box::pin(async move {
        pinned_peer.on_data_channel_open().await;
      })
    }));
    let on_message_peer = self.clone();
    data_channel.on_message(Box::new(move |msg| {
      let pinned_peer = on_message_peer.clone();
      Box::pin(async move {
        pinned_peer.on_data_channel_message(msg).await;
      })
    }));
    let on_error_peer = self.clone();
    data_channel.on_error(Box::new(move |error| {
      let pinned_peer = on_error_peer.clone();
      Box::pin(async move {
        pinned_peer.on_data_channel_error(error).await;
      })
    }));
    self
      .inner
      .data_channel
      .store(Ordering::Relaxed, data_channel);
  }

  async fn on_data_channel_open(&self) {
    self.internal_on_connect().await;
  }

  async fn on_data_channel_message(&self, msg: DataChannelMessage) {
    self.internal_on_data(msg.data.to_vec()).await;
  }

  async fn on_data_channel_error(&self, error: webrtc::Error) {
    eprintln!("data channel error: {}", error);
  }

  async fn internal_close(&self, emit: bool) -> Result<(), webrtc::Error> {
    if let Some(channel) = self.inner.data_channel.take(Ordering::SeqCst) {
      channel.close().await?;
    }
    if let Some(connection) = self.inner.connection.take(Ordering::SeqCst) {
      connection.close().await?;
    }
    if emit {
      self.internal_on_close().await;
    }
    Ok(())
  }

  pub async fn signal(&self, msg: SignalMessage) -> Result<(), webrtc::Error> {
    if self.inner.connection.is_none(Ordering::Relaxed) {
      self.create_peer().await?;
    }

    match msg {
      SignalMessage::Renegotiate => self.negotiate().await,
      SignalMessage::Candidate(candidate) => {
        if let Some(connection) = self.inner.connection.as_ref(Ordering::Relaxed) {
          if connection.remote_description().await.is_some() {
            return connection.add_ice_candidate(candidate).await;
          }
        }
        self.inner.pending_candidates.lock().await.push(candidate);
        Ok(())
      }
      SignalMessage::Answer(answer) => self.handle_session_description(answer).await,
      SignalMessage::Offer(offer) => self.handle_session_description(offer).await,
      SignalMessage::Pranswer(pranswer) => self.handle_session_description(pranswer).await,
      SignalMessage::Rollback(rollback) => self.handle_session_description(rollback).await,
    }
  }

  async fn handle_session_description(
    &self,
    sdp: RTCSessionDescription,
  ) -> Result<(), webrtc::Error> {
    if let Some(connection) = self.inner.connection.as_ref(Ordering::Relaxed) {
      let kind = sdp.sdp_type.clone();
      connection.set_remote_description(sdp).await?;
      for pending_candidate in self.inner.pending_candidates.lock().await.drain(..) {
        connection.add_ice_candidate(pending_candidate).await?;
      }
      if kind == RTCSdpType::Offer {
        self.create_answer().await?;
      }
      self.internal_on_negotiated().await;
      Ok(())
    } else {
      Err(webrtc::Error::ErrConnectionClosed)
    }
  }

  async fn create_offer(&self) -> Result<(), webrtc::Error> {
    if let Some(connection) = self.inner.connection.as_ref(Ordering::Relaxed) {
      let offer = connection
        .create_offer(self.inner.offer_config.clone())
        .await?;
      connection.set_local_description(offer.clone()).await?;
      self.internal_on_signal(SignalMessage::Offer(offer)).await;
    }
    Ok(())
  }

  async fn create_answer(&self) -> Result<(), webrtc::Error> {
    if let Some(connection) = self.inner.connection.as_ref(Ordering::Relaxed) {
      let answer = connection
        .create_answer(self.inner.answer_config.clone())
        .await?;
      connection.set_local_description(answer.clone()).await?;
      self.internal_on_signal(SignalMessage::Answer(answer)).await;
    }
    Ok(())
  }

  async fn negotiate(&self) -> Result<(), webrtc::Error> {
    if self.inner.initiator.load(Ordering::Relaxed) {
      return self.create_offer().await;
    }
    self.internal_on_signal(SignalMessage::Renegotiate).await;
    Ok(())
  }

  async fn internal_on_signal(&self, signal: SignalMessage) {
    if let Some(on_signal) = self.inner.on_signal.lock().await.as_mut() {
      on_signal(signal).await;
    }
  }
  async fn internal_on_data(&self, data: Vec<u8>) {
    if let Some(on_data) = self.inner.on_data.lock().await.as_mut() {
      on_data(data).await;
    }
  }
  async fn internal_on_connect(&self) {
    if let Some(on_connect) = self.inner.on_connect.lock().await.as_mut() {
      on_connect().await;
    }
  }
  async fn internal_on_close(&self) {
    if let Some(on_close) = self.inner.on_close.lock().await.as_mut() {
      on_close().await;
    }
  }
  async fn internal_on_negotiated(&self) {
    if let Some(on_negotiated) = self.inner.on_negotiated.lock().await.as_mut() {
      on_negotiated().await;
    }
  }
}

#[cfg(test)]
mod test {
  use webrtc::{
    api::{
      interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
    },
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
  };

  use super::*;

  #[tokio::test(flavor = "multi_thread")]
  async fn basic() -> Result<(), webrtc::Error> {
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
    peer1.on_signal(Box::new(move |singal| {
      let pinned_peer2 = on_signal_peer2.clone();
      Box::pin(async move {
        pinned_peer2
          .signal(singal)
          .await
          .expect("failed to signal peer2");
      })
    }));

    let on_signal_peer1 = peer1.clone();
    peer2.on_signal(Box::new(move |singal| {
      let pinned_peer1 = on_signal_peer1.clone();
      Box::pin(async move {
        pinned_peer1
          .signal(singal)
          .await
          .expect("failed to signal peer1");
      })
    }));

    let (connect_sender, mut connect_receiver) = tokio::sync::mpsc::channel::<()>(1);
    peer2.on_connect(Box::new(move || {
      let pinned_connect_sender = connect_sender.clone();
      Box::pin(async move {
        pinned_connect_sender
          .send(())
          .await
          .expect("failed to send connect");
      })
    }));

    let (message_sender, mut message_receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    peer1.on_data(Box::new(move |data| {
      let pinned_message_sender = message_sender.clone();
      Box::pin(async move {
        pinned_message_sender
          .send(data)
          .await
          .expect("failed to send connect");
      })
    }));

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
}
