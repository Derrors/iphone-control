//! Offline ProductType labels from DeviceKit at fab82bd988405367c915143e293479f481e93ba0.
//! See THIRD_PARTY.md for the source and license.

use super::DeviceInfo;

impl DeviceInfo {
    /// Classify even newly released hardware when lockdown reports a valid ProductType.
    pub fn family(&self) -> &'static str {
        let id = self.product_type.as_str();
        let (family, number) = if let Some(suffix) = id.strip_prefix("iPhone") {
            ("iphone", suffix)
        } else if let Some(suffix) = id.strip_prefix("iPad") {
            ("ipad", suffix)
        } else {
            return "unknown";
        };
        let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        match number.split_once(',') {
            Some((major, minor)) if numeric(major) && numeric(minor) => family,
            _ => "unknown",
        }
    }

    /// A known marketing name. Callers keep the original ProductType for diagnostics.
    pub fn model_name(&self) -> Option<&'static str> {
        Some(match self.product_type.as_str() {
            "iPhone3,1" | "iPhone3,2" | "iPhone3,3" => "iPhone 4",
            "iPhone4,1" => "iPhone 4s",
            "iPhone5,1" | "iPhone5,2" => "iPhone 5",
            "iPhone5,3" | "iPhone5,4" => "iPhone 5c",
            "iPhone6,1" | "iPhone6,2" => "iPhone 5s",
            "iPhone7,2" => "iPhone 6",
            "iPhone7,1" => "iPhone 6 Plus",
            "iPhone8,1" => "iPhone 6s",
            "iPhone8,2" => "iPhone 6s Plus",
            "iPhone9,1" | "iPhone9,3" => "iPhone 7",
            "iPhone9,2" | "iPhone9,4" => "iPhone 7 Plus",
            "iPhone8,4" => "iPhone SE",
            "iPhone10,1" | "iPhone10,4" => "iPhone 8",
            "iPhone10,2" | "iPhone10,5" => "iPhone 8 Plus",
            "iPhone10,3" | "iPhone10,6" => "iPhone X",
            "iPhone11,2" => "iPhone Xs",
            "iPhone11,4" | "iPhone11,6" => "iPhone Xs Max",
            "iPhone11,8" => "iPhone Xʀ",
            "iPhone12,1" => "iPhone 11",
            "iPhone12,3" => "iPhone 11 Pro",
            "iPhone12,5" => "iPhone 11 Pro Max",
            "iPhone12,8" => "iPhone SE (2nd generation)",
            "iPhone13,2" => "iPhone 12",
            "iPhone13,1" => "iPhone 12 mini",
            "iPhone13,3" => "iPhone 12 Pro",
            "iPhone13,4" => "iPhone 12 Pro Max",
            "iPhone14,5" => "iPhone 13",
            "iPhone14,4" => "iPhone 13 mini",
            "iPhone14,2" => "iPhone 13 Pro",
            "iPhone14,3" => "iPhone 13 Pro Max",
            "iPhone14,6" => "iPhone SE (3rd generation)",
            "iPhone14,7" => "iPhone 14",
            "iPhone14,8" => "iPhone 14 Plus",
            "iPhone15,2" => "iPhone 14 Pro",
            "iPhone15,3" => "iPhone 14 Pro Max",
            "iPhone15,4" => "iPhone 15",
            "iPhone15,5" => "iPhone 15 Plus",
            "iPhone16,1" => "iPhone 15 Pro",
            "iPhone16,2" => "iPhone 15 Pro Max",
            "iPhone17,3" => "iPhone 16",
            "iPhone17,4" => "iPhone 16 Plus",
            "iPhone17,1" => "iPhone 16 Pro",
            "iPhone17,2" => "iPhone 16 Pro Max",
            "iPhone17,5" => "iPhone 16e",
            "iPhone18,3" => "iPhone 17",
            "iPhone18,1" => "iPhone 17 Pro",
            "iPhone18,2" => "iPhone 17 Pro Max",
            "iPhone18,4" => "iPhone Air",
            "iPhone18,5" => "iPhone 17e",
            "iPhone19,2" => "iPhone 18 Pro",
            "iPhone19,3" | "iPhone19,7" => "iPhone 18 Pro Max",
            "iPhone19,4" => "iPhone Duo",
            "iPad2,1" | "iPad2,2" | "iPad2,3" | "iPad2,4" => "iPad 2",
            "iPad3,1" | "iPad3,2" | "iPad3,3" => "iPad (3rd generation)",
            "iPad3,4" | "iPad3,5" | "iPad3,6" => "iPad (4th generation)",
            "iPad4,1" | "iPad4,2" | "iPad4,3" => "iPad Air",
            "iPad5,3" | "iPad5,4" => "iPad Air 2",
            "iPad6,11" | "iPad6,12" => "iPad (5th generation)",
            "iPad7,5" | "iPad7,6" => "iPad (6th generation)",
            "iPad11,3" | "iPad11,4" => "iPad Air (3rd generation)",
            "iPad7,11" | "iPad7,12" => "iPad (7th generation)",
            "iPad11,6" | "iPad11,7" => "iPad (8th generation)",
            "iPad12,1" | "iPad12,2" => "iPad (9th generation)",
            "iPad13,18" | "iPad13,19" => "iPad (10th generation)",
            "iPad15,7" | "iPad15,8" => "iPad (A16)",
            "iPad13,1" | "iPad13,2" => "iPad Air (4th generation)",
            "iPad13,16" | "iPad13,17" => "iPad Air (5th generation)",
            "iPad14,8" | "iPad14,9" => "iPad Air (11-inch) (M2)",
            "iPad14,10" | "iPad14,11" => "iPad Air (13-inch) (M2)",
            "iPad15,3" | "iPad15,4" => "iPad Air (11-inch) (M3)",
            "iPad15,5" | "iPad15,6" => "iPad Air (13-inch) (M3)",
            "iPad16,8" | "iPad16,9" => "iPad Air (11-inch) (M4)",
            "iPad16,10" | "iPad16,11" => "iPad Air (13-inch) (M4)",
            "iPad2,5" | "iPad2,6" | "iPad2,7" => "iPad mini",
            "iPad4,4" | "iPad4,5" | "iPad4,6" => "iPad mini 2",
            "iPad4,7" | "iPad4,8" | "iPad4,9" => "iPad mini 3",
            "iPad5,1" | "iPad5,2" => "iPad mini 4",
            "iPad11,1" | "iPad11,2" => "iPad mini (5th generation)",
            "iPad14,1" | "iPad14,2" => "iPad mini (6th generation)",
            "iPad16,1" | "iPad16,2" => "iPad mini (A17 Pro)",
            "iPad6,3" | "iPad6,4" => "iPad Pro (9.7-inch)",
            "iPad6,7" | "iPad6,8" => "iPad Pro (12.9-inch)",
            "iPad7,1" | "iPad7,2" => "iPad Pro (12.9-inch) (2nd generation)",
            "iPad7,3" | "iPad7,4" => "iPad Pro (10.5-inch)",
            "iPad8,1" | "iPad8,2" | "iPad8,3" | "iPad8,4" => "iPad Pro (11-inch)",
            "iPad8,5" | "iPad8,6" | "iPad8,7" | "iPad8,8" => "iPad Pro (12.9-inch) (3rd generation)",
            "iPad8,9" | "iPad8,10" => "iPad Pro (11-inch) (2nd generation)",
            "iPad8,11" | "iPad8,12" => "iPad Pro (12.9-inch) (4th generation)",
            "iPad13,4" | "iPad13,5" | "iPad13,6" | "iPad13,7" => "iPad Pro (11-inch) (3rd generation)",
            "iPad13,8" | "iPad13,9" | "iPad13,10" | "iPad13,11" => "iPad Pro (12.9-inch) (5th generation)",
            "iPad14,3" | "iPad14,4" => "iPad Pro (11-inch) (4th generation)",
            "iPad14,5" | "iPad14,6" => "iPad Pro (12.9-inch) (6th generation)",
            "iPad16,3" | "iPad16,4" => "iPad Pro (11-inch) (M4)",
            "iPad16,5" | "iPad16,6" => "iPad Pro (13-inch) (M4)",
            "iPad17,1" | "iPad17,2" => "iPad Pro (11-inch) (M5)",
            "iPad17,3" | "iPad17,4" => "iPad Pro (13-inch) (M5)",
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::DeviceInfo;

    #[test]
    fn marketing_name_and_family_are_independent() {
        for (id, family, name) in [
            ("iPhone16,1", "iphone", Some("iPhone 15 Pro")),
            ("iPad13,1", "ipad", Some("iPad Air (4th generation)")),
            ("iPad13,2", "ipad", Some("iPad Air (4th generation)")),
            ("iPad99,1", "ipad", None),
            ("iPhone99,1", "iphone", None),
            ("iPhone", "unknown", None),
            ("iPhone,12", "unknown", None),
            ("iPad13,1,2", "unknown", None),
            ("Mac14,2", "unknown", None),
            ("", "unknown", None),
        ] {
            let device = DeviceInfo { product_type: id.into(), ..Default::default() };
            assert_eq!((device.family(), device.model_name()), (family, name), "{id}");
        }
    }
}
