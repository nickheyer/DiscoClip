use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Clip { url: Url },
    Status,
}
