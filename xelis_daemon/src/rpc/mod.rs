pub mod rpc;
pub mod websocket;
pub mod getwork_server;

use crate::core::{error::BlockchainError, blockchain::Blockchain};
use crate::rpc::getwork_server::GetWorkServer;
use crate::rpc::websocket::WebSocketHandler;
use actix::{Addr, MailboxError};
use actix_web::web::Path;
use actix_web::{get, post, web::{self, Payload}, error::Error, App, HttpResponse, HttpServer, Responder, dev::ServerHandle, ResponseError, HttpRequest};
use actix_web_actors::ws::WsResponseBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, Error as SerdeError, json};
use tokio::sync::Mutex;
use xelis_common::api::daemon::{NotifyEvent, EventResult};
use xelis_common::config;
use xelis_common::crypto::address::Address;
use xelis_common::serializer::ReaderError;
use std::borrow::Cow;
use std::{sync::Arc, collections::HashMap, pin::Pin, future::Future, fmt::{Display, Formatter}};
use log::{trace, info, debug};
use anyhow::Error as AnyError;
use thiserror::Error;
use self::getwork_server::{GetWorkWebSocketHandler, SharedGetWorkServer};
use self::websocket::Response;
use log::error;

pub type SharedRpcServer = web::Data<Arc<RpcServer>>;
pub type Handler = fn(Arc<Blockchain>, Value) -> Pin<Box<dyn Future<Output = Result<Value, RpcError>>>>;

pub const JSON_RPC_VERSION: &str = "2.0";

#[derive(Error, Debug)]
pub enum RpcError {
    #[error("Invalid body in request")]
    ParseBodyError,
    #[error("Invalid request")]
    InvalidRequest,
    #[error("Invalid params: {}", _0)]
    InvalidParams(#[from] SerdeError),
    #[error("Unexpected parameters for this method")]
    UnexpectedParams,
    #[error("Expected json_rpc set to '2.0'")]
    InvalidVersion,
    #[error("Method '{}' in request was not found", _0)]
    MethodNotFound(String),
    #[error(transparent)]
    BlockchainError(#[from] BlockchainError),
    #[error(transparent)]
    DeserializerError(#[from] ReaderError),
    #[error(transparent)]
    AnyError(#[from] AnyError),
    #[error("Error, expected a normal wallet address")]
    ExpectedNormalAddress,
    #[error("Error, no P2p enabled")]
    NoP2p,
    #[error("WebSocket client is not registered")]
    ClientNotRegistered,
    #[error("Could not send message to address: {}", _0)]
    WebSocketSendError(#[from] MailboxError),
}

impl RpcError {
    pub fn get_code(&self) -> i16 {
        match self {
            RpcError::ParseBodyError => -32700,
            RpcError::InvalidRequest | RpcError::InvalidVersion => -32600,
            RpcError::MethodNotFound(_) => -32601,
            RpcError::InvalidParams(_) | RpcError::UnexpectedParams => -32602,
            _ => -32603
        }
    }
}

#[derive(Debug)]
pub struct RpcResponseError {
    id: Option<usize>,
    error: RpcError
}

impl RpcResponseError {
    pub fn new(id: Option<usize>, error: RpcError) -> Self {
        Self {
            id,
            error
        }
    }

    pub fn get_id(&self) -> Value {
        match self.id {
            Some(id) => json!(id),
            None => Value::Null
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "jsonrpc": JSON_RPC_VERSION,
            "id": self.get_id(),
            "error": {
                "code": self.error.get_code(),
                "message": self.error.to_string()
            }
        })
    }
}

impl Display for RpcResponseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "RpcError[id: {}, error: {}]", self.get_id(), self.error.to_string())
    }
}

impl ResponseError for RpcResponseError {
    fn error_response(&self) -> HttpResponse {
        HttpResponse::Ok().json(self.to_json())
    }
}

#[derive(Deserialize)]
pub struct RpcRequest {
    jsonrpc: String,
    id: Option<usize>,
    method: String,
    params: Option<Value>
}

pub struct RpcServer {
    handle: Mutex<Option<ServerHandle>>, // keep the server handle to stop it gracefully
    methods: HashMap<String, Handler>, // all rpc methods registered
    blockchain: Arc<Blockchain>, // pointer to blockchain data
    clients: Mutex<HashMap<Addr<WebSocketHandler>, HashMap<NotifyEvent, Option<usize>>>>, // all websocket clients connected with subscriptions linked
    getwork: Option<SharedGetWorkServer>
}

impl RpcServer {
    pub async fn new(bind_address: String, blockchain: Arc<Blockchain>, disable_getwork_server: bool) -> Result<Arc<Self>, BlockchainError> {
        let getwork: Option<SharedGetWorkServer> = if !disable_getwork_server {
            info!("Creating GetWork server...");
            Some(Arc::new(GetWorkServer::new(blockchain.clone())))
        } else {
            None
        };

        let mut server = Self {
            handle: Mutex::new(None),
            methods: HashMap::new(),
            clients: Mutex::new(HashMap::new()),
            getwork,
            blockchain
        };
        rpc::register_methods(&mut server);

        let rpc_server = Arc::new(server);
        let rpc_clone = Arc::clone(&rpc_server);
        let server = HttpServer::new(move || {
            let rpc = Arc::clone(&rpc_clone);
            App::new()
                .app_data(web::Data::new(rpc))
                .service(index)
                .service(json_rpc)
                .service(ws_endpoint)
                .service(getwork_endpoint)
        })
        .disable_signals()
        .bind(&bind_address)?
        .run();
        
        {
            let handle = server.handle();
            *rpc_server.handle.lock().await = Some(handle);
        }

        // start the http server
        info!("RPC server will listen on: http://{}", bind_address);
        tokio::spawn(server);
        Ok(rpc_server)
    }

    pub async fn stop(&self) {
        info!("Stopping RPC Server...");
        if let Some(handler) = self.handle.lock().await.take() {
            handler.stop(false).await;
        }
        info!("RPC Server is now stopped!");
    }

    pub fn parse_request(&self, body: &[u8]) -> Result<RpcRequest, RpcResponseError> {
        let request: RpcRequest = serde_json::from_slice(&body).map_err(|_| RpcResponseError::new(None, RpcError::ParseBodyError))?;
        if request.jsonrpc != JSON_RPC_VERSION {
            return Err(RpcResponseError::new(request.id, RpcError::InvalidVersion));
        }
        Ok(request)
    }

    pub async fn execute_method(&self, mut request: RpcRequest) -> Result<Value, RpcResponseError> {
        let handler = match self.methods.get(&request.method) {
            Some(handler) => handler,
            None => return Err(RpcResponseError::new(request.id, RpcError::MethodNotFound(request.method)))
        };
        trace!("executing '{}' RPC method", request.method);
        let result = handler(Arc::clone(&self.blockchain), request.params.take().unwrap_or(Value::Null)).await.map_err(|err| RpcResponseError::new(request.id, err.into()))?;
        Ok(json!({
            "jsonrpc": JSON_RPC_VERSION,
            "id": request.id,
            "result": result
        }))
    }

    pub fn register_method(&mut self, name: &str, handler: Handler) {
        if self.methods.insert(name.into(), handler).is_some() {
            error!("The method '{}' was already registered !", name);
        }
    }

    pub fn get_blockchain(&self) -> &Arc<Blockchain> {
        &self.blockchain
    }

    pub async fn add_client(&self, addr: Addr<WebSocketHandler>) {
        let mut clients = self.clients.lock().await;
        clients.insert(addr, HashMap::new());
    }

    pub async fn remove_client(&self, addr: &Addr<WebSocketHandler>) {
        let mut clients = self.clients.lock().await;
        let deleted = clients.remove(addr).is_some();
        debug!("WebSocket client {:?} deleted: {}", addr, deleted);
    }

    pub async fn subscribe_client_to(&self, addr: &Addr<WebSocketHandler>, subscribe: NotifyEvent, id: Option<usize>) -> Result<(), RpcError> {
        let mut clients = self.clients.lock().await;
        let subscriptions = clients.get_mut(addr).ok_or_else(|| RpcError::ClientNotRegistered)?;
        subscriptions.insert(subscribe, id);
        Ok(())
    }

    pub async fn unsubscribe_client_from(&self, addr: &Addr<WebSocketHandler>, subscribe: &NotifyEvent) -> Result<(), RpcError> {
        let mut clients = self.clients.lock().await;
        let subscriptions = clients.get_mut(addr).ok_or_else(|| RpcError::ClientNotRegistered)?;
        subscriptions.remove(subscribe);
        Ok(())
    }

    // notify all clients connected to the websocket which have subscribed to the event sent.
    // each client message is sent through a tokio task in case an error happens and to prevent waiting on others clients
    pub async fn notify_clients<V: Serialize>(&self, notify: &NotifyEvent, value: V) -> Result<(), RpcError> {
        let value = json!(EventResult { event: Cow::Borrowed(notify), value: json!(value) });
        let clients = self.clients.lock().await;
        for (addr, subs) in clients.iter() {
            if let Some(id) = subs.get(notify) {
                let addr = addr.clone();
                let response = Response(json!({
                    "jsonrpc": JSON_RPC_VERSION,
                    "id": id,
                    "result": value
                }));
                tokio::spawn(async move {
                    match addr.send(response).await {
                        Ok(response) => {
                            if let Err(e) = response {
                                debug!("Error while sending websocket event: {} ", e);
                            } 
                        }
                        Err(e) => {
                            debug!("Error while sending on mailbox: {}", e);
                        }
                    };
                });
            }
        }
        Ok(())
    }

    pub fn getwork_server(&self) -> &Option<SharedGetWorkServer> {
        &self.getwork
    }
}

#[get("/")]
async fn index() -> impl Responder {
    HttpResponse::Ok().body(format!("Hello, world!\nRunning on: {}", config::VERSION))
}

// TODO support batch
#[post("/json_rpc")]
async fn json_rpc(rpc: SharedRpcServer, body: web::Bytes) -> Result<impl Responder, RpcResponseError> {
    let request = rpc.parse_request(&body)?;
    let result = rpc.execute_method(request).await?;
    Ok(HttpResponse::Ok().json(result))
}

#[get("/ws")]
async fn ws_endpoint(server: SharedRpcServer, request: HttpRequest, stream: Payload) -> Result<HttpResponse, Error> {
    let (addr, response) = WsResponseBuilder::new(WebSocketHandler::new(server.clone()), &request, stream).start_with_addr()?;
    trace!("New client connected to WebSocket: {:?}", addr);
    server.add_client(addr).await;

    Ok(response)
}

#[get("/getwork/{address}/{worker}")]
async fn getwork_endpoint(server: SharedRpcServer, request: HttpRequest, stream: Payload, path: Path<(String, String)>) -> Result<HttpResponse, Error> {
    match &server.getwork {
        Some(getwork) => {
            let (addr, worker) = path.into_inner();
            if worker.len() > 32 {
                return Ok(HttpResponse::BadRequest().reason("Worker name must be less or equal to 32 chars").finish())
            }

            let address: Address<'_> = match Address::from_string(&addr) {
                Ok(address) => address,
                Err(e) => {
                    debug!("Invalid miner address for getwork server: {}", e);
                    return Ok(HttpResponse::BadRequest().reason("Invalid miner address for getwork server").finish())
                }
            };
            if !address.is_normal() {
                return Ok(HttpResponse::BadRequest().reason("Address should be in normal format").finish())
            }

            if address.is_mainnet() != server.get_blockchain().get_network().is_mainnet() {
                return Ok(HttpResponse::BadRequest().reason("Address is not in same network state").finish())
            }

            let key = address.to_public_key();
            let (addr, response) = WsResponseBuilder::new(GetWorkWebSocketHandler::new(getwork.clone()), &request, stream).start_with_addr()?;
            trace!("New miner connected to GetWork WebSocket: {:?}", addr);
            getwork.add_miner(addr, key, worker).await;
            Ok(response)
        },
        None => Ok(HttpResponse::NotFound().reason("GetWork server is not enabled").finish()) // getwork server is not started
    }
}