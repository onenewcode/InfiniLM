use std::borrow::Cow;
use tokeneer::{utok, Tokeneer};

pub trait Tokenize {
    fn encode(&self, text: &str) -> Vec<utok>;
    fn decode(&self, token: utok) -> &str;
}

impl<M: tokeneer::Method> Tokenize for Tokeneer<M> {
    #[inline]
    fn encode(&self, text: &str) -> Vec<utok> {
        self.encode(text)
    }
    #[inline]
    fn decode(&self, token: utok) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.internal().decode(token)) }
    }
}

pub trait Normalizer {
    fn encode<'a>(&self, text: &'a str) -> Cow<'a, str>;
    fn decode<'a>(&self, text: &'a str) -> Cow<'a, str>;
}

impl Normalizer for () {
    #[inline]
    fn encode<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(text)
    }

    #[inline]
    fn decode<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(text)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BPECommonNormalizer;

impl Normalizer for BPECommonNormalizer {
    fn encode<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut ans = String::new();
        if text
            .chars()
            .next()
            .filter(char::is_ascii_alphabetic)
            .is_some()
        {
            ans.push('▁');
        }
        for c in text.chars() {
            ans.push(match c {
                ' ' => '▁',
                c => c,
            });
        }
        Cow::Owned(ans)
    }

    #[inline]
    fn decode<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if text.contains('▁') {
            Cow::Owned(text.replace('▁', " "))
        } else {
            Cow::Borrowed(text)
        }
    }
}
