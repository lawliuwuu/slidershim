use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use hyper::{
  header,
  server::conn::AddrStream,
  service::{make_service_fn, service_fn},
  upgrade::{self, Upgraded},
  Body, Method, Request, Response, Server, StatusCode,
};
use log::{error, info};
use path_clean::PathClean;
use std::{convert::Infallible, env::current_exe, future::Future, net::SocketAddr};
use tokio::{
  fs::File,
  select,
  sync::mpsc,
  time::{sleep, Duration},
};
use tokio_tungstenite::WebSocketStream;
use tokio_util::codec::{BytesCodec, FramedRead};
use tungstenite::{handshake, Message};

use crate::slider_io::{controller_state::FullState, worker::AsyncJob};

// https://levelup.gitconnected.com/handling-websocket-and-http-on-the-same-port-with-rust-f65b770722c9

async fn error_response() -> Result<Response<Body>, Infallible> {
  Ok(
    Response::builder()
      .status(StatusCode::NOT_FOUND)
      .body(Body::from(format!("Not found")))
      .unwrap(),
  )
}

async fn serve_file(path: &str) -> Result<Response<Body>, Infallible> {
  let mut pb = current_exe().unwrap();
  pb.pop();
  pb.push("res/www");
  pb.push(path);
  pb.clean();

  // println!("CWD {:?}", std::env::current_dir());

  match File::open(&pb).await {
    Ok(f) => {
      info!("Serving file {:?}", pb);
      let stream = FramedRead::new(f, BytesCodec::new());
      let body = Body::wrap_stream(stream);
      Ok(Response::new(body))
    }
    Err(_) => error_response().await,
  }
}

async fn handle_brokenithm(
  ws_stream: WebSocketStream<Upgraded>,
  state: FullState,
  led_enabled: bool,
) {
  let (mut ws_write, mut ws_read) = ws_stream.split();

  let (msg_write, mut msg_read) = mpsc::unbounded_channel::<Message>();

  let write_task = async move {
    // info!("Websocket write task open");
    loop {
      match msg_read.recv().await {
        Some(msg) => match ws_write.send(msg).await.ok() {
          Some(_) => {}
          None => {
            break;
          }
        },
        None => {
          break;
        }
      }
    }
    // info!("Websocket write task done");
  };

  let msg_write_handle = msg_write.clone();
  let state_handle = state.clone();
  let read_task = async move {
    // info!("Websocket read task open");
    loop {
      match ws_read.next().await {
        Some(msg) => match msg {
          Ok(msg) => match msg {
            Message::Text(msg) => {
              let mut chars = msg.chars();
              let head = chars.next().unwrap();
              match head {
                'a' => {
                  msg_write_handle
                    .send(Message::Text("alive".to_string()))
                    .ok();
                }
                'b' => {
                  let flat_state: Vec<bool> = chars
                    .map(|x| match x {
                      '0' => false,
                      '1' => true,
                      _ => unreachable!(),
                    })
                    .collect();
                  let mut controller_state_handle = state_handle.controller_state.lock().unwrap();
                  for (idx, c) in flat_state[0..32].iter().enumerate() {
                    controller_state_handle.ground_state[idx] = match c {
                      false => 0,
                      true => 255,
                    }
                  }
                  for (idx, c) in flat_state[32..38].iter().enumerate() {
                    controller_state_handle.air_state[idx] = match c {
                      false => 0,
                      true => 1,
                    }
                  }
                }
                _ => {
                  break;
                }
              }
            }
            Message::Close(_) => {
              info!("Websocket connection closed");
              break;
            }
            _ => {}
          },
          Err(e) => {
            error!("Websocket connection error: {}", e);
            break;
          }
        },
        None => {
          break;
        }
      }
    }
    // info!("Websocket read task done");
  };

  match led_enabled {
    false => {
      select! {
        _ = read_task => {}
        _ = write_task => {}
      };
    }
    true => {
      let msg_write_handle = msg_write.clone();
      let state_handle = state.clone();
      let led_task = async move {
        loop {
          let mut led_data = vec![0; 93];
          {
            let led_state_handle = state_handle.led_state.lock().unwrap();
            (&mut led_data).copy_from_slice(&led_state_handle.led_state);
          }
          msg_write_handle.send(Message::Binary(led_data)).ok();

          sleep(Duration::from_millis(50)).await;
        }
      };

      select! {
        _ = read_task => {}
        _ = write_task => {}
        _ = led_task => {}
      };
    }
  }
}

async fn handle_websocket(
  mut request: Request<Body>,
  state: FullState,
  led_enabled: bool,
) -> Result<Response<Body>, Infallible> {
  let res = match handshake::server::create_response_with_body(&request, || Body::empty()) {
    Ok(res) => {
      tokio::spawn(async move {
        match upgrade::on(&mut request).await {
          Ok(upgraded) => {
            let ws_stream = WebSocketStream::from_raw_socket(
              upgraded,
              tokio_tungstenite::tungstenite::protocol::Role::Server,
              None,
            )
            .await;

            handle_brokenithm(ws_stream, state, led_enabled).await;
          }

          Err(e) => {
            error!("Websocket upgrade error: {}", e);
          }
        }
      });

      res
    }
    Err(e) => {
      error!("Websocket creation error: {}", e);
      Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::from(format!("Failed to create websocket: {}", e)))
        .unwrap()
    }
  };
  Ok(res)
}

async fn handle_request(
  request: Request<Body>,
  remote_addr: SocketAddr,
  state: FullState,
  ground_only: bool,
  led_enabled: bool,
) -> Result<Response<Body>, Infallible> {
  let method = request.method();
  let path = request.uri().path();
  if method != Method::GET {
    error!(
      "Server unknown method {} -> {} {}",
      remote_addr, method, path
    );
    return error_response().await;
  }
  info!("Server {} -> {} {}", remote_addr, method, path);

  match (
    request.uri().path(),
    request.headers().contains_key(header::UPGRADE),
  ) {
    ("/", false) | ("/index.html", false) => match ground_only {
      false => serve_file("index.html").await,
      true => serve_file("index-go.html").await,
    },
    (filename, false) => serve_file(&filename[1..]).await,
    ("/ws", true) => handle_websocket(request, state, led_enabled).await,
    _ => error_response().await,
  }
}

pub struct BrokenithmJob {
  state: FullState,
  ground_only: bool,
  led_enabled: bool,
}

impl BrokenithmJob {
  pub fn new(state: &FullState, ground_only: &bool, led_enabled: &bool) -> Self {
    Self {
      state: state.clone(),
      ground_only: *ground_only,
      led_enabled: *led_enabled,
    }
  }
}

#[async_trait]
impl AsyncJob for BrokenithmJob {
  async fn run<F: Future<Output = ()> + Send>(self, stop_signal: F) {
    let state = self.state.clone();
    let ground_only = self.ground_only;
    let led_enabled = self.led_enabled;
    let make_svc = make_service_fn(|conn: &AddrStream| {
      let remote_addr = conn.remote_addr();
      let make_svc_state = state.clone();
      async move {
        Ok::<_, Infallible>(service_fn(move |request: Request<Body>| {
          let svc_state = make_svc_state.clone();
          handle_request(request, remote_addr, svc_state, ground_only, led_enabled)
        }))
      }
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], 1606));
    info!("Brokenithm server listening on {}", addr);

    let server = Server::bind(&addr)
      // .http1_keepalive(false)
      // .http2_keep_alive_interval(None)
      // .tcp_keepalive(None)
      .serve(make_svc)
      .with_graceful_shutdown(stop_signal);

    if let Err(e) = server.await {
      info!("Brokenithm server stopped: {}", e);
    }
  }
}
