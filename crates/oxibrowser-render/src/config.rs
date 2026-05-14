//! Configuration types for the rendering engine.

use crate::error::{RenderError, RenderResult};

/// Output format for rendered content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// PNG raster image.
    #[default]
    Png,
    /// PDF vector document.
    Pdf,
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputFormat::Png => write!(f, "png"),
            OutputFormat::Pdf => write!(f, "pdf"),
        }
    }
}

/// Color scheme preference for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    /// Light color scheme.
    #[default]
    Light,
    /// Dark color scheme.
    Dark,
}

impl From<ColorScheme> for blitz_traits::shell::ColorScheme {
    fn from(scheme: ColorScheme) -> Self {
        match scheme {
            ColorScheme::Light => blitz_traits::shell::ColorScheme::Light,
            ColorScheme::Dark => blitz_traits::shell::ColorScheme::Dark,
        }
    }
}

/// Rendering configuration.
///
/// Use the builder pattern to construct:
///
/// ```rust,ignore
/// let config = RenderConfig::new()
///     .width(1920)
///     .height(1080)
///     .scale(2.0)
///     .format(OutputFormat::Pdf);
/// ```
#[derive(Debug, Clone)]
pub struct RenderConfig {
    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
    /// Device pixel ratio scale factor.
    pub scale: f32,
    /// Output format (PNG or PDF).
    pub format: OutputFormat,
    /// Color scheme preference.
    pub color_scheme: ColorScheme,
    /// Automatically adjust height to fit content.
    pub auto_height: bool,
    /// Background color as RGBA (default: white).
    pub background: [u8; 4],
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            scale: 1.0,
            format: OutputFormat::Png,
            color_scheme: ColorScheme::Light,
            auto_height: false,
            background: [255, 255, 255, 255],
        }
    }
}

impl RenderConfig {
    /// Create a new config with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set viewport width.
    pub fn width(mut self, width: u32) -> Self {
        self.width = width;
        self
    }

    /// Set viewport height.
    pub fn height(mut self, height: u32) -> Self {
        self.height = height;
        self
    }

    /// Set viewport dimensions.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Set scale factor (1.0 = standard, 2.0 = retina).
    pub fn scale(mut self, scale: f32) -> Self {
        self.scale = scale;
        self
    }

    /// Set output format.
    pub fn format(mut self, format: OutputFormat) -> Self {
        self.format = format;
        self
    }

    /// Set color scheme preference.
    pub fn color_scheme(mut self, scheme: ColorScheme) -> Self {
        self.color_scheme = scheme;
        self
    }

    /// Enable automatic height detection from content.
    pub fn auto_height(mut self, auto: bool) -> Self {
        self.auto_height = auto;
        self
    }

    /// Set background color as RGBA.
    pub fn background(mut self, rgba: [u8; 4]) -> Self {
        self.background = rgba;
        self
    }

    /// Minimum supported viewport dimension.
    pub const MIN_DIMENSION: u32 = 16;

    /// Validate configuration values.
    pub fn validate(&self) -> RenderResult<()> {
        if self.width < Self::MIN_DIMENSION {
            return Err(RenderError::InvalidConfig(format!(
                "width must be at least {} pixels",
                Self::MIN_DIMENSION
            )));
        }
        if self.height < Self::MIN_DIMENSION {
            return Err(RenderError::InvalidConfig(format!(
                "height must be at least {} pixels",
                Self::MIN_DIMENSION
            )));
        }
        if self.scale <= 0.0 || !self.scale.is_finite() {
            return Err(RenderError::InvalidConfig(
                "scale must be a positive finite number".to_string(),
            ));
        }
        Ok(())
    }
}