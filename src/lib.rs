pub mod animation;
pub mod assets;
pub mod asusctl;
pub mod config;
pub mod engine;
pub mod matrix;
pub mod model;
pub mod render;
pub mod sensors;

pub use config::ConfigStore;
pub use engine::{EngineCommand, EngineHandle};
pub use model::{
	AppConfig, BatteryStyle, ContentArea, CycleSettings, DevicePolicy, DisplayProfile, Element, GifLayout, GifLoop, MatrixGeometry, MatrixModel, ElementKind, OverlayColor, ProfileTriggers, Trigger,
	ScrollDirection, TextMode, WindowState,
};
