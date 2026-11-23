mod bounds;
mod cases;
mod client;
mod credentials;
mod describe;
mod grid;
mod http;
mod socket;
mod symbols;
mod wire;

#[macro_export]
macro_rules! skip_without_credentials {
    ($case:expr, $credentials:expr) => {
        match $credentials {
            Some(credentials) => credentials,
            None => {
                println!(
                    "SKIP {}: no {} / {} / {} in the environment and no readable {} at the \
                     workspace root",
                    $case,
                    $crate::credentials::API_KEY_ENV,
                    $crate::credentials::APP_SECRET_ENV,
                    $crate::credentials::ACCESS_TOKEN_ENV,
                    $crate::credentials::CREDENTIALS_FILE
                );
                return;
            }
        }
    };
}
