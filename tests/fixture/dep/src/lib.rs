//! Shapes used by the lockdoc integration tests.

pub mod geometry {
    /// A circle centred on the origin.
    #[derive(Clone, Debug)]
    pub struct Circle {
        pub radius: f32,
        id: u32,
    }

    impl Circle {
        /// Makes a circle. Negative radii are clamped to zero.
        pub fn new(radius: f32) -> Self {
            Self {
                radius: radius.max(0.0),
                id: 0,
            }
        }

        /// The enclosed area.
        ///
        /// ```
        /// # use fixture_shapes::Circle;
        /// let a = Circle::new(1.0).area();
        /// ```
        pub fn area(&self) -> f32 {
            std::f32::consts::PI * self.radius * self.radius
        }

        pub fn id(&self) -> u32 {
            self.id
        }
    }

    /// Anything with an area.
    pub trait Area {
        /// The area in square units.
        fn area(&self) -> f32;

        /// Twice the area.
        fn double(&self) -> f32 {
            self.area() * 2.0
        }
    }

    impl Area for Circle {
        fn area(&self) -> f32 {
            Circle::area(self)
        }
    }

    pub enum Kind {
        Round,
        Polygon(u8),
        Custom { name: String },
    }
}

#[cfg(feature = "extra")]
pub mod extra {
    /// Only present with the `extra` feature.
    pub fn sparkle() {}
}

#[doc(hidden)]
pub mod __private {
    pub fn internal() {}
}

pub mod prelude {
    #[doc(hidden)]
    pub use crate::geometry::*;
}

pub use geometry::Circle;
