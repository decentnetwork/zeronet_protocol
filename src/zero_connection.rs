use std::{
  future::Future,
  io::{Read, Write},
  sync::{Arc, Mutex},
};

use tokio::time::{timeout, Duration};

use decentnet_protocol::{
  address::PeerAddr,
  message::{Request, RequestType, Response, ResponseType, ZeroMessage},
  templates::Handshake,
};

use crate::{async_connection::Connection, error::Error};

#[derive(Debug, Clone)]
pub struct ConnectionCfg {
  /// Timeout for the connection in seconds default to 10
  pub timeout:      u64,
  /// Timeout for the ping in seconds default to 10
  pub timeout_ping: u64,
  /// Timeout for the request in seconds default to 10
  pub timeout_req:  u64,
}

impl Default for ConnectionCfg {
  fn default() -> Self {
    Self {
      timeout:      10,
      timeout_ping: 10,
      timeout_req:  10,
    }
  }
}

pub struct ZeroConnection {
  /// A ZeroNet Protocol connection
  ///
  /// The ZeroNet Protocol is specified at
  /// https://zeronet.io/docs/help_zeronet/network_protocol/
  ///
  /// # Examples
  /// ```no_run
  /// use std::net::{TcpStream, TcpListener};
  /// use futures::executor::block_on;
  ///	use zeronet_protocol::{ZeroConnection};
  ///	use decentnet_protocol::{address::PeerAddr, message::ZeroMessage};
  /// use decentnet_protocol::message::ResponseType;
  /// use decentnet_protocol::templates::PingResponse;
  ///
  /// fn handle_connection(stream: TcpStream) {
  ///		let mut connection = ZeroConnection::new(Box::new(stream.try_clone().unwrap()), Box::new(stream)).unwrap();
  ///		let request = block_on(connection.recv()).unwrap();
  ///
  ///		let body = "Pong!".to_string();
  ///		block_on(connection.respond(request.req_id, ResponseType::Ping(PingResponse{body}))).unwrap();
  /// }
  ///
  /// fn main() {
  /// 	let listener = TcpListener::bind("127.0.0.1:15442").unwrap();
  ///
  /// 	for stream in listener.incoming() {
  /// 		if let Ok(stream) = stream {
  /// 			handle_connection(stream)
  /// 		}
  /// 	}
  /// }
  /// ```
  pub connection:     Connection<ZeroMessage>,
  pub next_req_id:    Arc<Mutex<usize>>,
  pub target_address: Option<PeerAddr>,
  pub cfg:            ConnectionCfg,
}

impl Clone for ZeroConnection {
  fn clone(&self) -> Self {
    Self {
      connection:     self.connection.clone(),
      next_req_id:    self.next_req_id.clone(),
      target_address: self.target_address.clone(),
      cfg:            self.cfg.clone(),
    }
  }
}

impl ZeroConnection {
  /// Creates a new ZeroConnection from a given reader and writer
  pub fn new(
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    cfg: Option<ConnectionCfg>,
  ) -> Result<ZeroConnection, Error> {
    let conn = Connection::new(reader, writer);
    let conn = ZeroConnection {
      connection:     conn,
      next_req_id:    Arc::new(Mutex::new(0)),
      target_address: None,
      cfg:            cfg.unwrap_or_default(),
    };

    Ok(conn)
  }

  /// Creates a new ZeroConnection from a given address
  pub fn from_address(address: PeerAddr) -> Result<ZeroConnection, Error> {
    let (reader, writer) = address.get_pair()?;
    let mut conn = ZeroConnection::new(reader, writer, None)?;
    conn.target_address = Some(address);
    Ok(conn)
  }

  /// Creates a new ZeroConnection from a given address
  pub async fn from_address_async(
    address: PeerAddr,
    cfg: Option<ConnectionCfg>,
  ) -> Result<ZeroConnection, Error> {
    let cfg = cfg.unwrap_or_default();
    let duration = Duration::from_secs(cfg.timeout_ping);
    if let Ok(res) = timeout(duration, address.get_pair_async()).await {
      if let Ok((reader, writer)) = res {
        let mut conn = ZeroConnection::new(reader, writer, Some(cfg))?;
        conn.target_address = Some(address);
        Ok(conn)
      } else {
        Err(Error::ConnectionFailure)
      }
    } else {
      Err(Error::ConnectionTimeout)
    }
  }

  /// Connect to an ip and port and perform the handshake,
  /// then return the ZeroConnection.
  pub fn connect(address: String) -> impl Future<Output = Result<ZeroConnection, Error>> {
    return async {
      let address = PeerAddr::parse(address)?;
      let mut connection = ZeroConnection::from_address(address.clone()).unwrap();

      let mut body = Handshake::default();
      body.target_address = Some(address.to_string());
      // TODO:
      // - by default peer_id should be empty string
      // - peer_id is only generated for clearnet peers
      body.peer_id = String::new();

      let _resp = connection
        .request("handshake", RequestType::Handshake(body))
        .await?;
      // TODO: update the connection with information from the handshake
      // - peer_id
      // - port
      // - switch to encrypted connection based on crypt_supported and crypt
      // - no need for use_bin_type, we won't support deprecated non-binary connections
      // - what do with onion address?

      Ok(connection)
    };
  }

  /// Returns a future that will read from the internal reader
  /// and attempt to decode valid ZeroMessages.
  /// The future returns the first Request that gets decoded.
  pub fn recv(&mut self) -> impl Future<Output = Result<Request, Error>> {
    let result = self.connection.recv();

    return async {
      match result.await {
        Err(err) => Err(err),
        Ok(ZeroMessage::Response(_)) => Err(Error::UnexpectedResponse),
        Ok(ZeroMessage::Request(req)) => Ok(req),
      }
    };
  }

  /// Respond to a request.
  /// The `body` variable is flattened into the ZeroMessage,
  /// therefore it should be an object, a map or a pair.
  pub fn respond(
    &mut self,
    to: usize,
    body: ResponseType,
  ) -> impl Future<Output = Result<(), Error>> {
    let message = ZeroMessage::response(to, body);
    self.connection.send(message)
  }

  /// Returns a future that will send a request with
  /// a new `req_id` and then read from internal reader
  /// and attempt to decode valid ZeroMessages.
  /// The future returns the first Response that
  /// has the corresponding `to` field.
  pub fn request(
    &mut self,
    cmd: &str,
    body: RequestType,
  ) -> impl Future<Output = Result<Response, Error>> {
    let message = ZeroMessage::request(cmd, self.req_id(), body);
    let result = timeout(
      Duration::from_secs(self.cfg.timeout_req),
      self.connection.request(message),
    );

    return async {
      match result.await {
        Err(err) => Err(err.into()),
        Ok(Ok(ZeroMessage::Response(res))) => Ok(res),
        _ => Err(Error::UnexpectedRequest),
      }
    };
  }

  /// Get the req_id of the last request
  pub fn last_req_id(&self) -> usize {
    let next_req_id = self.next_req_id.lock().unwrap();
    *next_req_id - 1
  }

  fn req_id(&mut self) -> usize {
    let mut next_req_id = self.next_req_id.lock().unwrap();
    *next_req_id += 1;
    *next_req_id - 1
  }
}
