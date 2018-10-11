extern crate actix_web;
extern crate futures;
extern crate mime;
extern crate mime_guess;
extern crate tempfile;
extern crate url;
extern crate uuid;

#[macro_use]
extern crate cfg_if;

mod utils {
    // Copied from app_dirs: https://docs.rs/app_dirs/1.2.1/src/app_dirs/utils.rs.html
    pub fn sanitized_filename(component: &str) -> String {
        let mut buf = String::with_capacity(component.len());
        for (i, c) in component.chars().enumerate() {
            let is_lower = 'a' <= c && c <= 'z';
            let is_upper = 'A' <= c && c <= 'Z';
            let is_letter = is_upper || is_lower;
            let is_number = '0' <= c && c <= '9';
            let is_space = c == ' ';
            let is_hyphen = c == '-';
            let is_underscore = c == '_';
            let is_period = c == '.' && i != 0; // Disallow accidentally hidden folders
            let is_valid =
                is_letter || is_number || is_space || is_hyphen || is_underscore || is_period;
            if is_valid {
                buf.push(c);
            } else {
                buf.push_str(&format!(",{},", c as u32));
            }
        }
        buf
    }
}

use tempfile::NamedTempFile;

use actix_web::{
    dev, error,
    http::header::{ContentDisposition, DispositionParam},
    multipart, Error, FromRequest, HttpMessage, HttpRequest
};

use futures::{future, Future, IntoFuture, Stream};

use std::{
    io::Write,
    path::{Path, PathBuf}
};

#[derive(Debug)]
pub struct Parts {
    texts: TextParts,
    files: FileParts
}

#[derive(Debug)]
pub struct TextParts(pub Vec<(String, String)>);
#[derive(Debug)]
pub struct FileParts(pub Vec<(String, File)>);

#[derive(Debug)]
pub enum Part {
    Text(String),
    File(File)
}

#[derive(Debug)]
pub struct File {
    pub inner: NamedTempFile,
    pub file_name: String
}

pub fn handle_multipart_item(
    item: multipart::MultipartItem<dev::Payload>
) -> Box<Stream<Item = Option<(String, Part)>, Error = Error>> {
    match item {
        multipart::MultipartItem::Field(field) => Box::new(handle_field(field).into_stream()),
        multipart::MultipartItem::Nested(mp) => Box::new(
            mp.map_err(error::ErrorInternalServerError).map(handle_multipart_item).flatten()
        )
    }
}

pub fn handle_field(
    field: multipart::Field<dev::Payload>
) -> Box<Future<Item = Option<(String, Part)>, Error = Error>> {
    let mut field_name_opt = None;
    let mut file_name_opt = None;

    for param in field.content_disposition().into_iter().flat_map(|x| x.parameters) {
        match param {
            DispositionParam::Name(s) => {
                field_name_opt = Some(s);
            }
            DispositionParam::Filename(s) => {
                file_name_opt = Some(s);
            }
            _ => {}
        }
    }

    let field_name = match field_name_opt {
        Some(s) => s,
        None => {
            return Box::new(future::ok(None));
        }
    };

    let content_type = field.content_type().clone();

    match (file_name_opt, content_type) {
        (None, ref mt) if mt == &mime::TEXT_PLAIN || mt == &mime::APPLICATION_OCTET_STREAM => {
            let rt = field
                .concat2()
                .and_then(move |bytes| {
                    let rt =
                        String::from_utf8(bytes.to_vec()).ok().map(|s| (field_name, Part::Text(s)));
                    future::ok(rt)
                }).map_err(error::ErrorInternalServerError);
            Box::new(rt)
        }
        (file_name_opt, mt) => {
            let file_name = match file_name_opt {
                Some(s) => s,
                None => {
                    let uuid = ::uuid::Uuid::new_v4().to_simple();
                    match ::mime_guess::get_mime_extensions(&mt).and_then(|x| x.first()) {
                        Some(ext) => format!("{}.{}", uuid, ext),
                        None => uuid.to_string()
                    }
                }
            };

            let mut file = match NamedTempFile::new() {
                Ok(file) => file,
                Err(e) => {
                    return Box::new(future::err(error::ErrorInternalServerError(e)));
                }
            };

            let rt = field
                .concat2()
                .and_then(move |bytes| {
                    let rt = file
                        .write_all(bytes.as_ref())
                        .map(|_| Some((field_name, Part::File(File { inner: file, file_name }))))
                        .map_err(|e| error::MultipartError::Payload(error::PayloadError::Io(e)));
                    future::result(rt)
                }).map_err(error::ErrorInternalServerError);

            Box::new(rt)
        }
    }
}

impl<T> FromRequest<T> for Parts {
    type Config = ();
    type Result = Box<Future<Item = Self, Error = Error>>;

    fn from_request(req: &HttpRequest<T>, _: &Self::Config) -> Self::Result {
        let parts = req
            .multipart()
            .map_err(error::ErrorInternalServerError)
            .map(move |mp| handle_multipart_item(mp))
            .flatten()
            .filter_map(|x| x)
            .collect()
            .map(|parts| {
                let mut texts = Vec::with_capacity(parts.len());
                let mut files = Vec::with_capacity(parts.len());
                for (name, p) in parts {
                    match p {
                        Part::Text(s) => {
                            texts.push((name, s));
                        }
                        Part::File(f) => {
                            files.push((name, f));
                        }
                    }
                }
                Parts { texts: TextParts(texts), files: FileParts(files) }
            });
        Box::new(parts)
    }
}

impl TextParts {
    pub fn to_query_string(&self) -> String {
        let mut qs = ::url::form_urlencoded::Serializer::new(String::new());

        for (key, val) in &self.0 {
            qs.append_pair(&key, &val);
        }

        qs.finish()
    }
}

impl FileParts {
    pub fn remove(&mut self, key: &str) -> Vec<File> {
        let mut taken = Vec::with_capacity(self.0.len());
        let mut untaken = Vec::with_capacity(self.0.len());

        for (k, v) in self.0.drain(..) {
            if k == key {
                taken.push(v);
            } else {
                untaken.push((k, v));
            }
        }

        self.0 = untaken;

        taken
    }
}

cfg_if! {
    if #[cfg(unix)] {
        impl File {
            fn persist<P: AsRef<Path>>(self, dir: P) -> Result<::std::fs::File, ::tempfile::PersistError> {
                use std::os::unix::fs::PermissionsExt;
                let permissions = ::std::fs::Permissions::from_mode(0o644);
                let _ = ::std::fs::set_permissions(self.inner.path(), permissions);
                self.inner.persist(&dir.as_ref().join(utils::sanitized_filename(&self.file_name)))
            }
        }
    } else {
        impl File {
            fn persist<P: AsRef<Path>>(self, dir: P) -> Result<::std::fs::File, ::tempfile::PersistError> {
                self.inner.persist(&dir.as_ref().join(utils::sanitized_filename(&self.file_name)))
            }
        }
    }
}
