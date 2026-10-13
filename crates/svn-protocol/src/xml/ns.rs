//! XML namespaces used by the Subversion HTTP protocol.

/// `DAV:` namespace (`D:`).
pub const DAV_URI: &str = "DAV:";
/// Subversion protocol namespace used by REPORT bodies (`S:`).
pub const SVN_URI: &str = "svn:";
/// Namespace of versioned Subversion properties (`svn:*`).
pub const SVN_PROPS_URI: &str = "http://subversion.tigris.org/xmlns/svn/";
/// Namespace of user (custom) properties.
pub const CUSTOM_PROPS_URI: &str = "http://subversion.tigris.org/xmlns/custom/";
/// Namespace of DAV-level Subversion properties (`svn:wc:*`, checksums, `deadprop-count`).
pub const SVN_DAV_URI: &str = "http://subversion.tigris.org/xmlns/dav/";
/// Namespace of mod_dav error elements (`m:human-readable`).
pub const APACHE_DAV_URI: &str = "http://apache.org/dav/xmlns";
const XML_URI: &str = "http://www.w3.org/XML/1998/namespace";

/// Resolved XML namespace of an element or attribute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ns {
    /// No namespace (unprefixed attribute, or no default namespace in scope).
    None,
    /// `DAV:`.
    Dav,
    /// `svn:` (REPORT vocabulary and `C:error`).
    Svn,
    /// Versioned Subversion properties.
    SvnProps,
    /// Custom (user) properties.
    Custom,
    /// DAV-level Subversion properties.
    SvnDav,
    /// mod_dav error vocabulary.
    Apache,
    /// The reserved `xml:` namespace.
    Xml,
    /// Any other namespace (kept verbatim, ignored by the parsers).
    Other(String),
}

impl Ns {
    /// Classifies a namespace URI.
    pub fn from_uri(uri: &str) -> Ns {
        match uri {
            "" => Ns::None,
            DAV_URI => Ns::Dav,
            SVN_URI => Ns::Svn,
            SVN_PROPS_URI => Ns::SvnProps,
            CUSTOM_PROPS_URI => Ns::Custom,
            SVN_DAV_URI => Ns::SvnDav,
            APACHE_DAV_URI => Ns::Apache,
            XML_URI => Ns::Xml,
            other => Ns::Other(other.to_owned()),
        }
    }
}

/// Namespace-qualified name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Name {
    /// Resolved namespace.
    pub ns: Ns,
    /// Local part (everything after the first colon of the lexical name).
    pub local: String,
}

impl Name {
    /// Whether this name is `local` in namespace `ns`.
    pub fn is(&self, ns: &Ns, local: &str) -> bool {
        &self.ns == ns && self.local == local
    }
}
