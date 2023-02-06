use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};

use hyper::body::to_bytes;
use hyper::http::HeaderValue;
use hyper::server::conn::AddrStream;
use mlua::prelude::*;

use hyper::service::Service;
use hyper::{Body, HeaderMap, Request, Response, Server};
use reqwest::{ClientBuilder, Method};
use tokio::sync::mpsc::Sender;
use tokio::task;

use crate::utils::{net::get_request_user_agent_header, table::TableBuilder};
use crate::LuneMessage;

pub fn create(lua: &Lua) -> LuaResult<()> {
    lua.globals().raw_set(
        "net",
        TableBuilder::new(lua)?
            .with_function("jsonEncode", net_json_encode)?
            .with_function("jsonDecode", net_json_decode)?
            .with_async_function("request", net_request)?
            .with_async_function("serve", net_serve)?
            .build_readonly()?,
    )
}

fn net_json_encode(_: &Lua, (val, pretty): (LuaValue, Option<bool>)) -> LuaResult<String> {
    if let Some(true) = pretty {
        serde_json::to_string_pretty(&val).map_err(LuaError::external)
    } else {
        serde_json::to_string(&val).map_err(LuaError::external)
    }
}

fn net_json_decode(lua: &Lua, json: String) -> LuaResult<LuaValue> {
    let json: serde_json::Value = serde_json::from_str(&json).map_err(LuaError::external)?;
    lua.to_value(&json)
}

async fn net_request<'lua>(lua: &'lua Lua, config: LuaValue<'lua>) -> LuaResult<LuaTable<'lua>> {
    // Extract stuff from config and make sure its all valid
    let (url, method, headers, body) = match config {
        LuaValue::String(s) => {
            let url = s.to_string_lossy().to_string();
            let method = "GET".to_string();
            Ok((url, method, HashMap::new(), None))
        }
        LuaValue::Table(tab) => {
            // Extract url
            let url = match tab.raw_get::<_, LuaString>("url") {
                Ok(config_url) => Ok(config_url.to_string_lossy().to_string()),
                Err(_) => Err(LuaError::RuntimeError(
                    "Missing 'url' in request config".to_string(),
                )),
            }?;
            // Extract method
            let method = match tab.raw_get::<_, LuaString>("method") {
                Ok(config_method) => config_method.to_string_lossy().trim().to_ascii_uppercase(),
                Err(_) => "GET".to_string(),
            };
            // Extract headers
            let headers = match tab.raw_get::<_, LuaTable>("headers") {
                Ok(config_headers) => {
                    let mut lua_headers = HashMap::new();
                    for pair in config_headers.pairs::<LuaString, LuaString>() {
                        let (key, value) = pair?.to_owned();
                        lua_headers.insert(key, value);
                    }
                    lua_headers
                }
                Err(_) => HashMap::new(),
            };
            // Extract body
            let body = match tab.raw_get::<_, LuaString>("body") {
                Ok(config_body) => Some(config_body.as_bytes().to_owned()),
                Err(_) => None,
            };
            Ok((url, method, headers, body))
        }
        value => Err(LuaError::RuntimeError(format!(
            "Invalid request config - expected string or table, got {}",
            value.type_name()
        ))),
    }?;
    // Convert method string into proper enum
    let method = method.trim().to_ascii_uppercase();
    let method = match method.as_ref() {
        "GET" => Ok(Method::GET),
        "POST" => Ok(Method::POST),
        "PUT" => Ok(Method::PUT),
        "DELETE" => Ok(Method::DELETE),
        "HEAD" => Ok(Method::HEAD),
        "OPTIONS" => Ok(Method::OPTIONS),
        "PATCH" => Ok(Method::PATCH),
        _ => Err(LuaError::RuntimeError(format!(
            "Invalid request config method '{}'",
            &method
        ))),
    }?;
    // TODO: Figure out how to reuse this client
    let mut default_headers = HeaderMap::new();
    default_headers.insert(
        "User-Agent",
        HeaderValue::from_str(&get_request_user_agent_header()).map_err(LuaError::external)?,
    );
    let client = ClientBuilder::new()
        .default_headers(default_headers)
        .build()
        .map_err(LuaError::external)?;
    // Create and send the request
    let mut request = client.request(method, &url);
    for (header, value) in headers {
        request = request.header(header.to_str()?, value.to_str()?);
    }
    let res = request
        .body(body.unwrap_or_default())
        .send()
        .await
        .map_err(LuaError::external)?;
    // Extract status, headers
    let res_status = res.status().as_u16();
    let res_status_text = res.status().canonical_reason();
    let res_headers = res
        .headers()
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_owned()))
        .collect::<HashMap<String, String>>();
    // Read response bytes
    let res_bytes = res.bytes().await.map_err(LuaError::external)?;
    // Construct and return a readonly lua table with results
    TableBuilder::new(lua)?
        .with_value("ok", (200..300).contains(&res_status))?
        .with_value("statusCode", res_status)?
        .with_value("statusMessage", res_status_text)?
        .with_value("headers", res_headers)?
        .with_value("body", lua.create_string(&res_bytes)?)?
        .build_readonly()
}

async fn net_serve<'lua>(
    lua: &'lua Lua,
    (port, callback): (u16, LuaFunction<'lua>),
) -> LuaResult<()> {
    let server_lua = lua.app_data_ref::<Weak<Lua>>().unwrap().upgrade().unwrap();
    let server_sender = lua
        .app_data_ref::<Weak<Sender<LuneMessage>>>()
        .unwrap()
        .upgrade()
        .unwrap();
    let server_callback = server_lua.create_registry_value(callback)?;
    let server = Server::bind(&([127, 0, 0, 1], port).into())
        .executor(LocalExec)
        .serve(MakeNetService(server_lua, server_callback.into()));
    if let Err(err) = server.await.map_err(LuaError::external) {
        server_sender
            .send(LuneMessage::LuaError(err))
            .await
            .map_err(LuaError::external)?;
    }
    Ok(())
}

// Hyper service implementation for net, lots of boilerplate here
// but make_svc and make_svc_function do not work for what we need

pub struct NetService(Arc<Lua>, Arc<LuaRegistryKey>);

impl Service<Request<Body>> for NetService {
    type Response = Response<Body>;
    type Error = LuaError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let lua = self.0.clone();
        let key = self.1.clone();
        let (parts, body) = req.into_parts();
        Box::pin(async move {
            // Convert request body into bytes, extract handler
            // function & lune message sender to use later
            let bytes = to_bytes(body).await.map_err(LuaError::external)?;
            let handler: LuaFunction = lua.registry_value(&key)?;
            let sender = lua
                .app_data_ref::<Weak<Sender<LuneMessage>>>()
                .unwrap()
                .upgrade()
                .unwrap();
            // Create a readonly table with request info to pass to the handler
            let request = TableBuilder::new(&lua)?
                .with_value("path", parts.uri.path())?
                .with_value("query", parts.uri.query().unwrap_or_default())?
                .with_value("method", parts.method.as_str())?
                .with_value(
                    "headers",
                    parts
                        .headers
                        .iter()
                        .map(|(name, value)| {
                            (name.to_string(), value.to_str().unwrap().to_string())
                        })
                        .collect::<HashMap<String, String>>(),
                )?
                .with_value("body", lua.create_string(&bytes)?)?
                .build_readonly()?;
            match handler.call_async(request).await {
                // Plain strings from the handler are plaintext responses
                Ok(LuaValue::String(s)) => Ok(Response::builder()
                    .status(200)
                    .header("Content-Type", "text/plain")
                    .body(Body::from(s.as_bytes().to_vec()))
                    .unwrap()),
                // Tables are more detailed responses with potential status, headers, body
                Ok(LuaValue::Table(t)) => {
                    let status = t.get::<_, Option<u16>>("status")?.unwrap_or(200);
                    let mut resp = Response::builder().status(status);

                    if let Some(headers) = t.get::<_, Option<LuaTable>>("headers")? {
                        for pair in headers.pairs::<String, LuaString>() {
                            let (h, v) = pair?;
                            resp = resp.header(&h, v.as_bytes());
                        }
                    }

                    let body = t
                        .get::<_, Option<LuaString>>("body")?
                        .map(|b| Body::from(b.as_bytes().to_vec()))
                        .unwrap_or_else(Body::empty);

                    Ok(resp.body(body).unwrap())
                }
                // If the handler returns an error, generate a 5xx response
                Err(err) => {
                    sender
                        .send(LuneMessage::LuaError(err.to_lua_err()))
                        .await
                        .map_err(LuaError::external)?;
                    Ok(Response::builder()
                        .status(500)
                        .body(Body::from("Internal Server Error"))
                        .unwrap())
                }
                // If the handler returns a value that is of an invalid type,
                // this should also be an error, so generate a 5xx response
                Ok(value) => {
                    sender
                        .send(LuneMessage::LuaError(LuaError::RuntimeError(format!(
                            "Expected net serve handler to return a value of type 'string' or 'table', got '{}'",
                            value.type_name()
                        ))))
                        .await
                        .map_err(LuaError::external)?;
                    Ok(Response::builder()
                        .status(500)
                        .body(Body::from("Internal Server Error"))
                        .unwrap())
                }
            }
        })
    }
}

struct MakeNetService(Arc<Lua>, Arc<LuaRegistryKey>);

impl Service<&AddrStream> for MakeNetService {
    type Response = NetService;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&mut self, _: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: &AddrStream) -> Self::Future {
        let lua = self.0.clone();
        let key = self.1.clone();
        Box::pin(async move { Ok(NetService(lua, key)) })
    }
}

#[derive(Clone, Copy, Debug)]
struct LocalExec;

impl<F> hyper::rt::Executor<F> for LocalExec
where
    F: std::future::Future + 'static, // not requiring `Send`
{
    fn execute(&self, fut: F) {
        task::spawn_local(fut);
    }
}