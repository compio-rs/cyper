use url::Url;

/// A trait to try to convert some type into a [`Url`].
pub trait IntoUrl {
    /// Besides parsing as a valid [`Url`], the [`Url`] must be a valid
    /// `http::Uri`, in that it makes sense to use in a network request.
    fn into_url(self) -> crate::Result<Url>;

    /// Get the URL as a string slice.
    fn as_str(&self) -> &str;
}

impl IntoUrl for Url {
    fn into_url(self) -> crate::Result<Url> {
        if self.has_host() {
            Ok(self)
        } else {
            Err(crate::Error::InvalidUrl(self))
        }
    }

    fn as_str(&self) -> &str {
        self.as_ref()
    }
}

impl IntoUrl for &str {
    fn into_url(self) -> crate::Result<Url> {
        Ok(Url::parse(self)?)
    }

    fn as_str(&self) -> &str {
        self
    }
}

impl IntoUrl for &String {
    fn into_url(self) -> crate::Result<Url> {
        (&**self).into_url()
    }

    fn as_str(&self) -> &str {
        self
    }
}

impl IntoUrl for String {
    fn into_url(self) -> crate::Result<Url> {
        (&*self).into_url()
    }

    fn as_str(&self) -> &str {
        self
    }
}
