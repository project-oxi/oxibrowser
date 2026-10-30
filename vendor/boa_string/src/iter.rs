use std::iter::FusedIterator;

use crate::{CodePoint, JsStr};

use super::JsStrVariant;

#[derive(Debug, Clone)]
enum IterInner<'a> {
    U8(std::iter::Copied<std::slice::Iter<'a, u8>>),
    U16(std::iter::Copied<std::slice::Iter<'a, u16>>),
}

/// Iterator over a [`JsStr`].
#[derive(Debug, Clone)]
pub struct Iter<'a> {
    inner: IterInner<'a>,
}

impl<'a> Iter<'a> {
    #[inline]
    pub(crate) fn new(s: JsStr<'a>) -> Self {
        let inner = match s.variant() {
            JsStrVariant::Latin1(s) => IterInner::U8(s.iter().copied()),
            JsStrVariant::Utf16(s) => IterInner::U16(s.iter().copied()),
        };
        Iter { inner }
    }
}

impl Iterator for Iter<'_> {
    type Item = u16;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            IterInner::U8(iter) => iter.map(u16::from).next(),
            IterInner::U16(iter) => iter.next(),
        }
    }
}

impl FusedIterator for Iter<'_> {}

impl ExactSizeIterator for Iter<'_> {
    #[inline]
    fn len(&self) -> usize {
        match &self.inner {
            IterInner::U8(v) => v.len(),
            IterInner::U16(v) => v.len(),
        }
    }
}

#[derive(Debug, Clone)]
enum WindowsInner<'a> {
    U8(std::slice::Windows<'a, u8>),
    U16(std::slice::Windows<'a, u16>),
}

/// An iterator over overlapping subslices of length size.
///
/// This struct is created by the `windows` method.
#[derive(Debug, Clone)]
pub struct Windows<'a> {
    inner: WindowsInner<'a>,
}

impl<'a> Windows<'a> {
    #[inline]
    pub(crate) fn new(string: JsStr<'a>, size: usize) -> Self {
        let inner = match string.variant() {
            JsStrVariant::Latin1(v) => WindowsInner::U8(v.windows(size)),
            JsStrVariant::Utf16(v) => WindowsInner::U16(v.windows(size)),
        };
        Self { inner }
    }
}

impl<'a> Iterator for Windows<'a> {
    type Item = JsStr<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            WindowsInner::U8(iter) => iter.next().map(JsStr::latin1),
            WindowsInner::U16(iter) => iter.next().map(JsStr::utf16),
        }
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        // Preserve slice::Windows' constant-time advance. The default nth
        // walks the skipped prefix again for every StringIndexOf search.
        match &mut self.inner {
            WindowsInner::U8(iter) => iter.nth(n).map(JsStr::latin1),
            WindowsInner::U16(iter) => iter.nth(n).map(JsStr::utf16),
        }
    }
}

impl FusedIterator for Windows<'_> {}

impl ExactSizeIterator for Windows<'_> {
    #[inline]
    fn len(&self) -> usize {
        match &self.inner {
            WindowsInner::U8(v) => v.len(),
            WindowsInner::U16(v) => v.len(),
        }
    }
}

#[derive(Debug, Clone)]
enum CodePointsIterInner<'a> {
    Latin1(std::iter::Copied<std::slice::Iter<'a, u8>>),
    Utf16(std::char::DecodeUtf16<std::iter::Copied<std::slice::Iter<'a, u16>>>),
}

#[derive(Debug, Clone)]
pub struct CodePointsIter<'a> {
    inner: CodePointsIterInner<'a>,
}

impl<'a> CodePointsIter<'a> {
    #[inline]
    pub(crate) fn new(s: JsStr<'a>) -> Self {
        let inner = match s.variant() {
            JsStrVariant::Latin1(s) => CodePointsIterInner::Latin1(s.iter().copied()),
            JsStrVariant::Utf16(s) => {
                CodePointsIterInner::Utf16(char::decode_utf16(s.iter().copied()))
            }
        };
        CodePointsIter { inner }
    }
}

impl Iterator for CodePointsIter<'_> {
    type Item = CodePoint;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            CodePointsIterInner::Latin1(iter) => {
                iter.next().map(|b| CodePoint::Unicode(char::from(b)))
            }
            CodePointsIterInner::Utf16(iter) => iter.next().map(|res| match res {
                Ok(c) => CodePoint::Unicode(c),
                Err(e) => CodePoint::UnpairedSurrogate(e.unpaired_surrogate()),
            }),
        }
    }
}

impl FusedIterator for CodePointsIter<'_> {}

#[cfg(test)]
#[allow(clippy::iter_nth_zero)] // Exercise nth(0), not only the next() default.
mod windows_nth_tests {
    use super::Windows;
    use crate::JsStr;

    fn check_consumption(source: JsStr<'_>) {
        let mut windows = Windows::new(source, 2);
        assert_eq!(windows.len(), 4);
        assert_eq!(windows.nth(1), source.get(1..3));
        assert_eq!(windows.len(), 2);
        assert_eq!(windows.next(), source.get(2..4));
        assert_eq!(windows.len(), 1);
        assert_eq!(windows.nth(0), source.get(3..5));
        assert_eq!(windows.len(), 0);
        assert_eq!(windows.next(), None);
        assert_eq!(windows.nth(usize::MAX), None);
    }

    #[test]
    fn nth_preserves_latin1_and_utf16_code_units_and_consumption() {
        check_consumption(JsStr::latin1(&[0, 0x80, 0xff, b'\n', 0]));
        // Windows operate on code units, including paired/unpaired surrogates.
        check_consumption(JsStr::utf16(&[0x41, 0xd800, 0xdc00, 0x5a, 0xdfff]));
    }

    #[test]
    fn nth_out_of_range_exhausts_both_encodings_and_stays_fused() {
        for source in [JsStr::latin1(b"abcde"), JsStr::utf16(&[1, 2, 3, 4, 5])] {
            for skip in [4, 5, usize::MAX] {
                let mut windows = Windows::new(source, 2);
                assert_eq!(windows.nth(skip), None);
                assert_eq!(windows.len(), 0);
                assert_eq!(windows.next(), None);
            }
            let mut windows = Windows::new(source, 2);
            assert_eq!(windows.nth(3), source.get(3..5));
            assert_eq!(windows.next(), None);
            assert_eq!(Windows::new(source, 6).nth(0), None);
        }
        assert_eq!(Windows::new(JsStr::latin1(b""), 1).nth(0), None);
        assert_eq!(Windows::new(JsStr::utf16(&[]), 1).nth(usize::MAX), None);
    }

    #[test]
    fn index_of_respects_from_index_and_mixed_encoding_needles() {
        let latin1 = JsStr::latin1(&[b'a', 0x80, 0xff, b'\n', b'a']);
        assert_eq!(latin1.index_of(JsStr::utf16(&[0xff]), 1), Some(2));
        assert_eq!(latin1.index_of(JsStr::latin1(b"a"), 1), Some(4));
        assert_eq!(latin1.index_of(JsStr::latin1(b"a"), usize::MAX), None);
        let utf16 = JsStr::utf16(&[0x61, 0xd800, 0x0a, 0xff, 0x0a]);
        assert_eq!(utf16.index_of(JsStr::latin1(b"\n"), 3), Some(4));
        assert_eq!(utf16.index_of(JsStr::utf16(&[0xd800]), 1), Some(1));
        assert_eq!(utf16.index_of(JsStr::latin1(b""), 5), Some(5));
        assert_eq!(utf16.index_of(JsStr::latin1(b""), 6), None);
        assert_eq!(utf16.index_of(JsStr::latin1(b"a"), 5), None);
    }

    #[test]
    #[should_panic(expected = "window size must be non-zero")]
    fn zero_window_size_still_panics() {
        let _ = Windows::new(JsStr::latin1(b"abc"), 0);
    }
}
