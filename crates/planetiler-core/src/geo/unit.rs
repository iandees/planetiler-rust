//! Port of `geo/Unit.java`: units of length and area measurement.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::geo::geometry_exception::GeometryResult;
use crate::geo::with_geometry::WithGeometry;

fn normalize(unit: &str) -> String {
    let lower: String = unit.to_lowercase().chars().filter(|c| c.is_ascii_lowercase()).collect();
    lower.strip_suffix('s').map(str::to_string).unwrap_or(lower)
}

/// The base units that all other units are derived from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Base {
    /// Size of a feature in "z0 tiles" where 1 = the length/width/area of the entire world.
    Z0Tile,
    /// Size of a feature in meters.
    Meter,
}

impl Base {
    pub fn area<G: WithGeometry + ?Sized>(&self, geometry: &G) -> GeometryResult<f64> {
        match self {
            Base::Z0Tile => geometry.area(),
            Base::Meter => geometry.area_meters(),
        }
    }

    pub fn length<G: WithGeometry + ?Sized>(&self, geometry: &G) -> GeometryResult<f64> {
        match self {
            Base::Z0Tile => geometry.length(),
            Base::Meter => geometry.length_meters(),
        }
    }
}

/// Units to measure line length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Length {
    Meter,
    Foot,
    Yard,
    NauticalMile,
    Mile,
    Kilometer,
    Z0Pixel,
    Z0Tile,
}

const METERS_PER_FOOT: f64 = 0.3048;
const METERS_PER_YARD: f64 = 0.9144;
const METERS_PER_MILE: f64 = 1609.344;
const METERS_PER_NAUTICAL_MILE: f64 = 1852.0;
const SQUARE_METERS_PER_ACRE: f64 = 4046.8564224;

impl Length {
    pub const ALL: [Length; 8] = [
        Length::Meter,
        Length::Foot,
        Length::Yard,
        Length::NauticalMile,
        Length::Mile,
        Length::Kilometer,
        Length::Z0Pixel,
        Length::Z0Tile,
    ];

    /// The Java enum constant name.
    pub fn name(&self) -> &'static str {
        match self {
            Length::Meter => "METER",
            Length::Foot => "FOOT",
            Length::Yard => "YARD",
            Length::NauticalMile => "NAUTICAL_MILE",
            Length::Mile => "MILE",
            Length::Kilometer => "KILOMETER",
            Length::Z0Pixel => "Z0_PIXEL",
            Length::Z0Tile => "Z0_TILE",
        }
    }

    /// The full name of this unit.
    pub fn unit_name(&self) -> &'static str {
        match self {
            Length::Meter => "Metre",
            Length::Foot => "Foot",
            Length::Yard => "Yard",
            Length::NauticalMile => "Nautical mile",
            Length::Mile => "Mile",
            Length::Kilometer => "Kilometer",
            Length::Z0Pixel => "Z0 Pixel",
            Length::Z0Tile => "Z0 Tile",
        }
    }

    fn aliases(&self) -> &'static [&'static str] {
        match self {
            Length::Meter => &["m"],
            Length::Foot => &["ft", "feet"],
            Length::Yard => &["yd"],
            Length::NauticalMile => &["nm"],
            Length::Mile => &["mi", "miles"],
            Length::Kilometer => &["km"],
            Length::Z0Pixel => &["z0_px"],
            Length::Z0Tile => &["z0_ti"],
        }
    }

    /// The aliases for this unit (including the full name and enum name).
    pub fn symbols(&self) -> Vec<String> {
        let mut result: Vec<String> = Vec::new();
        for s in self.aliases().iter().copied().chain([self.unit_name(), self.name()]) {
            if !result.iter().any(|r| r == s) {
                result.push(s.to_string());
            }
        }
        result
    }

    pub fn base(&self) -> Base {
        match self {
            Length::Z0Pixel | Length::Z0Tile => Base::Z0Tile,
            _ => Base::Meter,
        }
    }

    fn multiplier(&self) -> f64 {
        match self {
            Length::Meter => 1.0,
            Length::Foot => 1.0 / METERS_PER_FOOT,
            Length::Yard => 1.0 / METERS_PER_YARD,
            Length::NauticalMile => 1.0 / METERS_PER_NAUTICAL_MILE,
            Length::Mile => 1.0 / METERS_PER_MILE,
            Length::Kilometer => 1e-3,
            Length::Z0Pixel => 1.0 / 256.0,
            Length::Z0Tile => 1.0,
        }
    }

    /// Converts a measurement in [`Base`] units to this unit.
    pub fn from_base_unit(&self, base: f64) -> f64 {
        base * self.multiplier()
    }

    /// Computes the length of `geometry` in this unit.
    pub fn of<G: WithGeometry + ?Sized>(&self, geometry: &G) -> GeometryResult<f64> {
        Ok(self.from_base_unit(self.base().length(geometry)?))
    }

    /// Look up a unit by name or alias (case-insensitive, ignoring non-letters and trailing `s`).
    pub fn from(label: &str) -> anyhow::Result<Length> {
        LENGTH_NAMES
            .get(&normalize(label))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Could not find area unit for '{label}'"))
    }
}

/// Units to measure polygon areas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Area {
    SquareMeter,
    SquareFoot,
    SquareYard,
    SquareNauticalMile,
    SquareMile,
    SquareKilometer,
    SquareZ0Pixel,
    SquareZ0Tile,
    Are,
    Hectare,
    Acre,
}

impl Area {
    pub const ALL: [Area; 11] = [
        Area::SquareMeter,
        Area::SquareFoot,
        Area::SquareYard,
        Area::SquareNauticalMile,
        Area::SquareMile,
        Area::SquareKilometer,
        Area::SquareZ0Pixel,
        Area::SquareZ0Tile,
        Area::Are,
        Area::Hectare,
        Area::Acre,
    ];

    fn length(&self) -> Option<Length> {
        match self {
            Area::SquareMeter => Some(Length::Meter),
            Area::SquareFoot => Some(Length::Foot),
            Area::SquareYard => Some(Length::Yard),
            Area::SquareNauticalMile => Some(Length::NauticalMile),
            Area::SquareMile => Some(Length::Mile),
            Area::SquareKilometer => Some(Length::Kilometer),
            Area::SquareZ0Pixel => Some(Length::Z0Pixel),
            Area::SquareZ0Tile => Some(Length::Z0Tile),
            _ => None,
        }
    }

    /// The Java enum constant name.
    pub fn name(&self) -> String {
        match self {
            Area::Are => "ARE".into(),
            Area::Hectare => "HECTARE".into(),
            Area::Acre => "ACRE".into(),
            other => format!("SQUARE_{}", other.length().unwrap().name()),
        }
    }

    /// The full name of this unit.
    pub fn unit_name(&self) -> String {
        match self {
            Area::Are => "Are".into(),
            Area::Hectare => "Hectare".into(),
            Area::Acre => "Acre".into(),
            other => format!("Square {}", other.length().unwrap().unit_name()),
        }
    }

    /// The aliases for this unit.
    pub fn symbols(&self) -> Vec<String> {
        match self {
            Area::Are => vec!["a".into()],
            Area::Hectare => vec!["ha".into()],
            Area::Acre => vec!["ac".into()],
            other => other
                .length()
                .unwrap()
                .symbols()
                .iter()
                .flat_map(|s| [format!("s{s}"), format!("square {s}"), format!("sq {s}"), format!("{s}2")])
                .collect(),
        }
    }

    pub fn base(&self) -> Base {
        self.length().map_or(Base::Meter, |l| l.base())
    }

    fn multiplier(&self) -> f64 {
        match self {
            Area::Are => 0.01,
            Area::Hectare => 0.0001,
            Area::Acre => 1.0 / SQUARE_METERS_PER_ACRE,
            other => {
                let m = other.length().unwrap().multiplier();
                m * m
            }
        }
    }

    /// Converts a measurement in [`Base`] units to this unit.
    pub fn from_base_unit(&self, base: f64) -> f64 {
        base * self.multiplier()
    }

    /// Computes the area of `geometry` in this unit.
    pub fn of<G: WithGeometry + ?Sized>(&self, geometry: &G) -> GeometryResult<f64> {
        Ok(self.from_base_unit(self.base().area(geometry)?))
    }

    /// Look up a unit by name or alias (case-insensitive, ignoring non-letters and trailing `s`).
    pub fn from(label: &str) -> anyhow::Result<Area> {
        AREA_NAMES
            .get(&normalize(label))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Could not find area unit for '{label}'"))
    }

    /// Look up a unit by its Java enum constant name (`Area.valueOf`).
    pub fn value_of(name: &str) -> Option<Area> {
        Area::ALL.into_iter().find(|a| a.name() == name)
    }
}

static LENGTH_NAMES: LazyLock<HashMap<String, Length>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    for unit in Length::ALL {
        for label in unit.symbols().into_iter().chain([unit.unit_name().to_string(), unit.name().to_string()]) {
            map.insert(normalize(&label), unit);
        }
    }
    map
});

static AREA_NAMES: LazyLock<HashMap<String, Area>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    for unit in Area::ALL {
        for label in unit.symbols().into_iter().chain([unit.unit_name(), unit.name()]) {
            map.insert(normalize(&label), unit);
        }
    }
    map
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_and_derived_area() {
        for (name, expected, aliases) in [
            ("METER", 1.0, "m; meters; metre; metres"),
            ("KILOMETER", 0.001, "km; kilometers"),
            ("FOOT", 3.28084, "ft; feet; foot"),
            ("MILE", 0.000621371, "mi; mile; miles"),
            ("NAUTICAL_MILE", 0.000539957, "nm; nautical miles"),
            ("YARD", 1.0936136964129, "yd; yards; yds"),
            ("Z0_PIXEL", 0.00390625, "z0px; z0 pixels; z0 pixel"),
            ("Z0_TILE", 1.0, "z0ti; z0tile; z0 tile; z0_tiles; z0_tiles; z0 tiles"),
        ] {
            let length = Length::from(name).unwrap();
            let area = Area::from(&format!("SQUARE_{name}")).unwrap();
            assert!((expected - length.from_base_unit(1.0)).abs() <= expected / 1e5, "{name}");
            let expected_area = expected * expected;
            assert!((expected_area - area.from_base_unit(1.0)).abs() <= expected_area / 1e5, "{name}");
            for alias in aliases.split(';') {
                assert_eq!(length, Length::from(alias).unwrap(), "{alias}");
                assert_eq!(area, Area::from(&format!("s{alias}")).unwrap(), "s{alias}");
                assert_eq!(area, Area::from(&format!("sq {alias}")).unwrap(), "sq {alias}");
                assert_eq!(area, Area::from(&format!("square {alias}")).unwrap(), "square {alias}");
                assert_eq!(area, Area::from(&format!("{alias}2")).unwrap(), "{alias}2");
            }
        }
    }

    #[test]
    fn custom_area() {
        for (name, expected, aliases) in
            [("ARE", 0.01, "a; ares"), ("HECTARE", 0.0001, "ha; hectares"), ("ACRE", 0.000247105, "ac; acres")]
        {
            let area = Area::value_of(name).unwrap();
            assert!((expected - area.from_base_unit(1.0)).abs() <= expected / 1e5);
            for alias in aliases.split(';') {
                assert_eq!(area, Area::from(alias).unwrap());
            }
        }
    }

    #[test]
    fn unknown() {
        assert!(Length::from("furlong").is_err());
        assert!(Area::from("furlong").is_err());
    }
}
