// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    error,
    fmt::{Debug, Display},
    io::Error as IoError,
    result,
};

use cloud::error::Error as CloudError;
use error_code::{self, ErrorCode, ErrorCodeExt};
use openssl::error::ErrorStack as CrypterError;
use protobuf::ProtobufError;
use thiserror::Error;
use tikv_util::stream::RetryError;

pub trait RetryCodedError: Debug + Display + ErrorCodeExt + RetryError + Send + Sync {}

/// The error type for encryption.
#[derive(Debug, Error)]
pub enum Error {
    #[error("Other error {0}")]
    Other(#[from] Box<dyn error::Error + Sync + Send>),
    // Only when the parsing record is the last one, and the
    // length is insufficient or the crc checksum fails.
    #[error("Recoverable tail record corruption while parsing file dictionary")]
    TailRecordParseIncomplete,
    // Currently only in use by cloud KMS
    #[error("Cloud KMS error {0}")]
    RetryCodedError(Box<dyn RetryCodedError>),
    #[error("RocksDB error {0}")]
    Rocks(String),
    #[error("IO error {0}")]
    Io(#[from] IoError),
    #[error("OpenSSL error {0}")]
    Crypter(#[from] CrypterError),
    #[error("Protobuf error {0}")]
    Proto(#[from] ProtobufError),
    #[error("Unknown encryption error")]
    UnknownEncryption,
    #[error("Wrong master key error {0}")]
    WrongMasterKey(Box<dyn error::Error + Sync + Send>),
    #[error("Both master key failed, current key {0}, previous key {1}.")]
    BothMasterKeyFail(
        Box<dyn error::Error + Sync + Send>,
        Box<dyn error::Error + Sync + Send>,
    ),
}

macro_rules! impl_from {
    ($($inner:ty => $container:ident,)+) => {
        $(
            impl From<$inner> for Error {
                fn from(inr: $inner) -> Error {
                    Error::$container(inr)
                }
            }
        )+
    };
}

impl_from! {
    String => Rocks,
}

impl From<Error> for IoError {
    fn from(err: Error) -> IoError {
        match err {
            Error::Io(e) => e,
            other => IoError::other(format!("{}", other)),
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

impl ErrorCodeExt for Error {
    fn error_code(&self) -> ErrorCode {
        match self {
            Error::RetryCodedError(err) => err.error_code(),
            Error::TailRecordParseIncomplete => error_code::encryption::PARSE_INCOMPLETE,
            Error::Rocks(_) => error_code::encryption::ROCKS,
            Error::Io(_) => error_code::encryption::IO,
            Error::Crypter(_) => error_code::encryption::CRYPTER,
            Error::Proto(_) => error_code::encryption::PROTO,
            Error::UnknownEncryption => error_code::encryption::UNKNOWN_ENCRYPTION,
            Error::WrongMasterKey(_) => error_code::encryption::WRONG_MASTER_KEY,
            Error::BothMasterKeyFail(..) => error_code::encryption::BOTH_MASTER_KEY_FAIL,
            Error::Other(_) => error_code::UNKNOWN,
        }
    }
}

impl RetryError for Error {
    fn is_retryable(&self) -> bool {
        // This should be refined.
        // However, only Error::Tls should be encountered
        match self {
            Error::RetryCodedError(err) => err.is_retryable(),
            Error::TailRecordParseIncomplete => true,
            Error::Rocks(_) => true,
            Error::Io(_) => true,
            Error::Crypter(_) => true,
            Error::Proto(_) => true,
            Error::UnknownEncryption => true,
            Error::WrongMasterKey(_) => false,
            Error::BothMasterKeyFail(..) => false,
            Error::Other(_) => true,
        }
    }
}

// CloudConverError adapts cloud errors to encryption errors
// As the abstract RetryCodedError
#[derive(Debug)]
pub struct CloudConvertError(CloudError, String);

impl RetryCodedError for CloudConvertError {}

impl std::fmt::Display for CloudConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{} {}", &self.0, &self.1))
    }
}

impl std::convert::From<CloudConvertError> for Error {
    fn from(err: CloudConvertError) -> Error {
        Error::RetryCodedError(Box::new(err) as Box<dyn RetryCodedError>)
    }
}

impl RetryError for CloudConvertError {
    fn is_retryable(&self) -> bool {
        self.0.is_retryable()
    }
}

impl ErrorCodeExt for CloudConvertError {
    fn error_code(&self) -> ErrorCode {
        self.0.error_code()
    }
}

pub fn cloud_convert_error(msg: String) -> Box<dyn FnOnce(CloudError) -> CloudConvertError> {
    Box::new(|err: CloudError| CloudConvertError(err, msg))
}
