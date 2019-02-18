// Copyright (C) 2019 Stephane Raux. Distributed under the MIT license.

#![deny(warnings)]

use clap::{App, Arg};
use futures::{Future, Stream};
use futures::future;
use http::{Request, Response, StatusCode};
use hyper::{Body, Server};
use hyper::service::service_fn;
use mime::Mime;
use nestxml::html;
use percent_encoding::percent_decode;
use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::fs::{DirEntry, Metadata};
use std::io::{self, Write};
use std::net::{AddrParseError, IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

const APP_NAME: &str = env!("CARGO_PKG_NAME");
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const APP_AUTHORS: &str = env!("CARGO_PKG_AUTHORS");

fn main() {
    if let Err(e) = run() {
        print_error(&e);
        std::process::exit(1)
    }
}

fn print_error(mut e: &dyn Error) {
    eprintln!("Error: {}", e);
    while let Some(cause) = e.source() {
        eprintln!("Because: {}", cause);
        e = cause;
    }
}

#[derive(Debug)]
enum AppError {
    BadAddress(AddrParseError),
    BadPort,
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AppError::BadAddress(_) => f.write_str("Invalid address"),
            AppError::BadPort => f.write_str("Invalid port"),
        }
    }
}

impl Error for AppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            AppError::BadAddress(e) => Some(e),
            AppError::BadPort => None,
        }
    }
}

fn run() -> Result<(), AppError> {
    let mut address = IpAddr::from(Ipv4Addr::UNSPECIFIED);
    let address_help = format!("IP address to listen on (default: {})",
        address);
    let mut port = 80_u16;
    let port_help = format!("Port to listen on (default: {})", port);
    let matches = App::new(APP_NAME)
        .version(APP_VERSION)
        .author(APP_AUTHORS)
        .about("Serves a directory over HTTP")
        .arg(
            Arg::with_name("DIRECTORY")
                .help("Directory to serve")
                .required(true)
        )
        .arg(
            Arg::with_name("address")
                .help(&address_help)
                .short("a")
                .long("address")
                .takes_value(true)
        )
        .arg(
            Arg::with_name("port")
                .help(&port_help)
                .short("p")
                .long("port")
                .takes_value(true)
        )
        .get_matches();
    let dir = PathBuf::from(matches.value_of("DIRECTORY").unwrap());
    if let Some(a) = matches.value_of("address") {
        address = a.parse().map_err(AppError::BadAddress)?;
    }
    if let Some(p) = matches.value_of("port") {
        port = p.parse().map_err(|_| AppError::BadPort)?;
    }
    let endpoint = (address, port).into();
    println!("Serving {} over HTTP on {}", dir.display(), endpoint);
    let new_service = move || {
        let root = dir.clone();
        service_fn(move |req| process_request(&root, req))
    };
    let (term_sender, term_receiver) = futures::sync::oneshot::channel();
    let term_sender = Cell::new(Some(term_sender));
    let _ = ctrlc::set_handler(move || {
        if let Some(sender) = term_sender.take() {
            let _ = sender.send(());
        }
    });
    let term_receiver = term_receiver.then(|_| {
        println!("Graceful shutdown requested");
        Ok::<(), ()>(())
    });
    let server = Server::bind(&endpoint)
        .serve(new_service)
        .with_graceful_shutdown(term_receiver);
    hyper::rt::run(server.map_err(|e| eprintln!("Server error: {}", e)));
    Ok(())
}

type ServerFuture<T> = Box<dyn Future<Item = T, Error = http::Error> + Send>;

fn process_request(root: &Path, request: Request<Body>)
    -> ServerFuture<Response<Body>>
{
    let req_path = percent_decode(request.uri().path().as_bytes());
    let req_path = match req_path.decode_utf8() {
        Ok(p) => p,
        Err(_) => return bad_request(),
    };
    let req_path = Path::new(&*req_path);
    let resource = match req_path.strip_prefix("/") {
        Ok(p) => p,
        Err(_) => return bad_request(),
    };
    let goes_up = resource.components().any(|part| match part {
        std::path::Component::ParentDir
            | std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            => true,
        _ => false,
    });
    if goes_up {return bad_request()}
    let path = root.join(resource);
    if !path.starts_with(root) {return bad_request()}
    let meta = match path.metadata() {
        Ok(meta) => meta,
        Err(e) => return io_error(e),
    };
    if meta.is_dir() {
        send_dir(&path, req_path)
    } else {
        send_file(path, meta)
    }
}

fn send_dir(path: &Path, req_path: &Path) -> ServerFuture<Response<Body>> {
    let entries = match read_dir(path) {
        Ok(entries) => entries,
        Err(e) => return io_error(e),
    };
    let page = format_file_list(&entries, req_path);
    let res = Response::builder().body(page.into());
    Box::new(future::result(res))
}

fn read_dir(path: &Path) -> Result<Vec<DirEntry>, io::Error> {
    path.read_dir()?.collect()
}

fn send_file(path: PathBuf, meta: Metadata) -> ServerFuture<Response<Body>> {
    let content_type = get_content_type(&path);
    let resp = tokio_fs::File::open(path)
        .map(move |file| {
            let chunks = tokio_codec::FramedRead::new(file,
                tokio_codec::BytesCodec::new());
            let chunks = chunks.map(|buf| buf.freeze());
            let body = Body::wrap_stream(chunks);
            Response::builder()
                .header(http::header::CONTENT_LENGTH, meta.len())
                .header(http::header::CONTENT_TYPE, content_type.to_string())
                .body(body)
                .unwrap()
        })
        .or_else(io_error);
    Box::new(resp)
}

fn get_content_type(p: &Path) -> Mime {
    let ext = match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => ext,
        None => return mime::APPLICATION_OCTET_STREAM,
    };
    match ext {
        "css" => mime::TEXT_CSS_UTF_8,
        "htm" | "html" => mime::TEXT_HTML_UTF_8,
        "json" => mime::APPLICATION_JSON,
        "txt" => mime::TEXT_PLAIN_UTF_8,
        "xml" => mime::TEXT_XML,
        _ => mime::APPLICATION_OCTET_STREAM,
    }
}

fn bad_request() -> ServerFuture<Response<Body>> {
    let res = Response::builder().status(StatusCode::BAD_REQUEST)
        .body("Bad request".into());
    Box::new(future::result(res))
}

fn io_error(e: io::Error) -> ServerFuture<Response<Body>> {
    let code = match e.kind() {
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let res = Response::builder().status(code)
        .body(format!("IO error: {}", e).into());
    Box::new(future::result(res))
}

fn format_file_list(entries: &[DirEntry], req_path: &Path) -> String {
    let mut out = Vec::<u8>::new();
    write_page(&mut out, "Directory contents", |out| {
        write_file_list(entries, req_path, out)
    }).unwrap();
    String::from_utf8(out).unwrap()
}

fn write_file_list<W: Write>(entries: &[DirEntry], req_path: &Path,
    out: &mut xml::EventWriter<W>) -> Result<(), xml::writer::Error>
{
    write_dir_title(req_path, out)?;
    html::table(out).write(|out| {
        html::tr(out).write(|out| {
            html::th(out).text("Filename")?;
            html::th(out).attr("class", "size").text("Size")
        })?;
        for (filename, entry) in entries.iter()
            .filter_map(|entry| entry.path().file_name()
                .and_then(|s| s.to_str())
                .map(|s| (s.to_owned(), entry))
            )
        {
            let rel_path = match req_path.join(&filename).to_str() {
                Some(s) => s.to_owned(),
                None => continue,
            };
            #[cfg(windows)]
            let rel_path = rel_path.replace("\\", "/");
            html::tr(out).write(|out| {
                html::td(out).write(|out| {
                    html::a(out).attr("href", rel_path).text(&filename)
                })?;
                let size = entry.metadata().ok().and_then(|meta| {
                    if meta.is_file() {
                        Some(pretty_size(meta.len()))
                    } else {
                        None
                    }
                });
                let size = size.unwrap_or(String::new());
                html::td(out).attr("class", "size").text(&size)
            })?;
        }
        Ok(())
    })
}

fn write_dir_title<W: Write>(path: &Path, out: &mut xml::EventWriter<W>)
    -> Result<(), xml::writer::Error>
{
    let mut parts = path.ancestors()
        .map(|p| {
            let file_name = p.file_name().and_then(|s| s.to_str())
                .unwrap_or("");
            (p.to_str().unwrap_or(""), file_name)
        })
        .collect::<Vec<_>>();
    parts.pop();
    parts.reverse();
    html::h1(out).write(|out| {
        out.write("Contents of ")?;
        html::a(out).attr("href", "/").text("/")?;
        for (link, name) in parts {
            html::a(out).attr("href", link).text(name)?;
            out.write("/")?;
        }
        Ok(())
    })
}

fn write_page<W, F>(out: W, title: &str, f: F) -> Result<(), xml::writer::Error>
where
    W: Write,
    F: FnOnce(&mut xml::EventWriter<W>) -> Result<(), xml::writer::Error>,
{
    let mut out = xml::EmitterConfig::new()
        .perform_indent(true)
        .write_document_declaration(false)
        .create_writer(out);
    const STYLESHEET: &str = include_str!("../data/style.css");
    html::write_doctype(&mut out)?;
    html::html(&mut out).write(|out| {
        html::head(out).write(|out| {
            html::title(out).text(title)?;
            html::meta(out).attr("charset", "UTF-8").empty()?;
            html::style(out).text(STYLESHEET)
        })?;
        html::body(out).write(f)
    })?;
    Ok(())
}

fn pretty_size(size: u64) -> String {
    match number_prefix::decimal_prefix(size as f64) {
        number_prefix::Standalone(x) => format!("{} B", x),
        number_prefix::Prefixed(prefix, x) => format!("{:.1} {}B", x, prefix),
    }
}
