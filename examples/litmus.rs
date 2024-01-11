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
        let mut config = DavHandler::builder(if !directory.is_empty() {
            FileSystem::local(directory, true, true, true)
        } else {
            FileSystem::Mem
        });
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
    #[arg(short, long, default_value = "4918")]
    port: u16,
    /// local directory to serve
    #[arg(short, long)]
    dir: Option<String>,
    /// serve from ephemeral memory filesystem
    #[arg(short, long)]
    memfs: bool,
    /// use ephemeral memory locksystem
    #[arg(short = 'l', long)]
    memls: bool,
    /// use fake memory locksystem
    #[arg(short, long)]
    fakels: bool,
    /// require basic authentication
    #[arg(short, long)]
    auth: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    let Cli {
        port,
        dir,
        memfs,
        memls,
        fakels,
        auth,
    } = Cli::parse();
    serve(dir, memfs, memls, fakels, auth, port).await
}

async fn serve(
    dir: Option<String>,
    memfs: bool,
    memls: bool,
    fakels: bool,
    auth: bool,
    port: u16,
) -> Result<(), Box<dyn Error>> {
    let (dir, name) = match dir.as_ref() {
        Some(dir) => (dir.as_str(), dir.as_str()),
        None => ("", "memory filesystem"),
    };
    let memls = memfs || memls;
    let fakels = fakels;

    let dav_server = Server::new(dir.into(), memls, fakels, auth);
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

    let port = port;
    let addr = format!("0.0.0.0:{}", port);
    let addr = SocketAddr::from_str(&addr)?;

    let server = hyper::Server::try_bind(&addr)?
        .serve(make_service)
        .map_err(|e| eprintln!("server error: {}", e));

    println!("Serving {} on {}", name, port);
    let _ = server.await;
    Ok(())
}

#[cfg(test)]
#[cfg(target_family = "unix")]
mod test {
    use std::env::current_dir;
    use std::path::PathBuf;
    use std::process::Command;

    use tokio::sync::Mutex;

    /// Prevent parallel installations.
    static INSTALL_LOCK: Mutex<()> = Mutex::const_new(());

    /// Download, build, and install litmus if not installed already.
    async fn install_litmus() -> PathBuf {
        const URL: &str = "http://www.webdav.org/neon/litmus/";
        const VERSION: &str = "0.13";
        let name = format!("litmus-{VERSION}");
        let litmus_dir = current_dir().unwrap().join(name.clone());
        println!("{litmus_dir:?}");

        let _lock = INSTALL_LOCK.lock().await;
        if !litmus_dir.exists() {
            let archive = format!("{name}.tar.gz");
            let url = format!("{URL}{archive}");

            let status = Command::new("curl")
                .arg("-O")
                .arg(url)
                .status()
                .expect("curl");
            assert!(status.success());

            let status = Command::new("tar")
                .arg("xf")
                .arg(archive.clone())
                .status()
                .expect("tar");
            assert!(status.success());
            let archive = current_dir().unwrap().join(archive);
            std::fs::remove_file(archive).unwrap();

            assert!(litmus_dir.exists());
            let status = Command::new("./configure")
                .current_dir(&litmus_dir)
                .status()
                .expect("configure");
            assert!(status.success());

            let status = Command::new("make")
                .current_dir(&litmus_dir)
                .status()
                .expect("make");
            assert!(status.success());
        }

        litmus_dir
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn directory() {
        let _ = env_logger::builder().is_test(true).try_init();

        let litmus_dir = install_litmus().await;

        std::fs::create_dir_all("tmp").unwrap();
        let server = tokio::spawn(async move {
            super::serve(Some("tmp".into()), true, false, false, true, 4918)
                .await
                .unwrap();
        });

        let status = Command::new("./litmus")
            .current_dir(litmus_dir)
            .env("TESTS", "http basic copymove locks props")
            .env("HTDOCS", "htdocs")
            .env("TESTROOT", ".")
            .arg("http://localhost:4918/")
            .arg("someuser")
            .arg("somepass")
            .status()
            .expect("litmus failed");

        if !status.success() {
            log::warn!("Localfs might not complete litmus");
        }

        server.abort();
        std::fs::remove_dir_all("tmp").unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn memory() {
        let _ = env_logger::builder().is_test(true).try_init();

        let litmus_dir = install_litmus().await;

        let server = tokio::spawn(async move {
            super::serve(None, true, false, false, true, 4919)
                .await
                .unwrap();
        });

        let status = Command::new("./litmus")
            .current_dir(litmus_dir)
            .env("TESTS", "http basic copymove locks props")
            .env("HTDOCS", "htdocs")
            .env("TESTROOT", ".")
            .arg("http://localhost:4919/")
            .arg("someuser")
            .arg("somepass")
            .status()
            .expect("litmus failed");

        assert!(status.success(), "Memfs should pass litmus!");

        server.abort();
    }
}
