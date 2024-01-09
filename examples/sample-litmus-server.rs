//
//  Sample application.
//
//  Listens on localhost:4918, plain http, no ssl.
//  Connect to http://localhost:4918/
//

use std::convert::Infallible;
use std::error::Error;
use std::net::SocketAddr;
use std::str::FromStr;

use clap::Parser;
use futures_util::future::TryFutureExt;
use headers::{authorization::Basic, Authorization, HeaderMapExt};

use dav_server::{body::Body, DavHandler, FileSystem, LockSystem};

#[derive(Clone)]
struct Server {
    dh: DavHandler,
    auth: bool,
}

impl Server {
    pub fn new(directory: String, memls: bool, fakels: bool, auth: bool) -> Self {
        let mut config = DavHandler::builder();
        if !directory.is_empty() {
            config = config.filesystem(FileSystem::local(directory, true, true, true));
        } else {
            config = config.filesystem(FileSystem::Mem);
        };
        if fakels {
            config = config.locksystem(LockSystem::Fake);
        }
        if memls {
            config = config.locksystem(LockSystem::Mem);
        }

        Server {
            dh: config.build(),
            auth,
        }
    }

    async fn handle(
        &self,
        req: hyper::Request<hyper::Body>,
    ) -> Result<hyper::Response<Body>, Infallible> {
        let user = if self.auth {
            // we want the client to authenticate.
            match req.headers().typed_get::<Authorization<Basic>>() {
                Some(Authorization(basic)) => Some(basic.username().to_string()),
                None => {
                    // return a 401 reply.
                    let response = hyper::Response::builder()
                        .status(401)
                        .header("WWW-Authenticate", "Basic realm=\"foo\"")
                        .body(Body::from("please auth".to_string()))
                        .unwrap();
                    return Ok(response);
                }
            }
        } else {
            None
        };

        if let Some(user) = user {
            Ok(self.dh.handle_with(req, None, Some(user)).await)
        } else {
            Ok(self.dh.handle(req).await)
        }
    }
}

#[derive(Debug, clap::Parser)]
#[command(about, version)]
struct Cli {
    /// port to listen on
    #[arg(short = 'p', long, default_value = "4918")]
    port: u16,
    /// local directory to serve
    #[arg(short = 'd', long)]
    dir: Option<String>,
    /// serve from ephemeral memory filesystem
    #[arg(short = 'm', long)]
    memfs: bool,
    /// use ephemeral memory locksystem
    #[arg(short = 'l', long)]
    memls: bool,
    /// use fake memory locksystem
    #[arg(short = 'f', long)]
    fakels: bool,
    /// require basic authentication
    #[arg(short = 'a', long)]
    auth: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    let args = Cli::parse();

    let (dir, name) = match args.dir.as_ref() {
        Some(dir) => (dir.as_str(), dir.as_str()),
        None => ("", "memory filesystem"),
    };
    let auth = args.auth;
    let memls = args.memfs || args.memls;
    let fakels = args.fakels;

    let dav_server = Server::new(dir.to_string(), memls, fakels, auth);
    let make_service = hyper::service::make_service_fn(|_| {
        let dav_server = dav_server.clone();
        async move {
            let func = move |req| {
                let dav_server = dav_server.clone();
                async move { dav_server.clone().handle(req).await }
            };
            Ok::<_, hyper::Error>(hyper::service::service_fn(func))
        }
    });

    let port = args.port;
    let addr = format!("0.0.0.0:{}", port);
    let addr = SocketAddr::from_str(&addr)?;

    let server = hyper::Server::try_bind(&addr)?
        .serve(make_service)
        .map_err(|e| eprintln!("server error: {}", e));

    println!("Serving {} on {}", name, port);
    let _ = server.await;
    Ok(())
}
