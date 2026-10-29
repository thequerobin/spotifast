//! Calendar periods use UTC, matching Spotify's `added_at` timestamps.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Period {
    pub start: String,
    pub end: String,
}

fn days(year: u32, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 31,
    }
}

fn boundary(input: &str, end: bool) -> Result<String, String> {
    let parts: Vec<_> = input.split('/').collect();
    let widths: &[usize] = match parts.len() {
        1 => &[4],
        2 => &[2, 4],
        3 => &[2, 2, 4],
        _ => return Err("Use YYYY, MM/YYYY or DD/MM/YYYY".into()),
    };
    if !parts
        .iter()
        .zip(widths)
        .all(|(p, n)| p.len() == *n && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("Use YYYY, MM/YYYY or DD/MM/YYYY".into());
    }
    let numbers: Vec<u32> = parts.iter().map(|p| p.parse().unwrap_or(0)).collect();
    let year = *numbers.last().unwrap_or(&0);
    let month = if numbers.len() == 1 {
        if end { 12 } else { 1 }
    } else {
        numbers[numbers.len() - 2]
    };
    if year == 0 || !(1..=12).contains(&month) {
        return Err("Invalid calendar date".into());
    }
    let day = if numbers.len() == 3 {
        numbers[0]
    } else if end {
        days(year, month)
    } else {
        1
    };
    if day == 0 || day > days(year, month) {
        return Err("Invalid calendar date".into());
    }
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

impl Period {
    /// Spotify release dates carry day, month or year precision in ISO form.
    pub fn release_period(value: &str) -> Option<Self> {
        let parts: Vec<_> = value.split('-').collect();
        let input = match parts.as_slice() {
            [year] => year.to_string(),
            [year, month] => format!("{month}/{year}"),
            [year, month, day] => format!("{day}/{month}/{year}"),
            _ => return None,
        };
        let period = Self {
            start: input,
            end: String::new(),
        };
        period.bounds().ok()?;
        Some(period)
    }

    /// Unknown day/month is a calendar interval, never a fabricated exact date.
    pub fn overlaps_release(&self, value: &str) -> bool {
        let Some(release) = Self::release_period(value) else {
            return false;
        };
        let Ok((low, high)) = self.bounds() else {
            return false;
        };
        let Ok((Some(release_low), Some(release_high))) = release.bounds() else {
            return false;
        };
        low.as_ref().is_none_or(|low| &release_high >= low)
            && high.as_ref().is_none_or(|high| &release_low <= high)
    }

    /// Keep indexed discovery bounded. Broad/open periods use the catalog.
    pub fn search_years(&self) -> Vec<u16> {
        let Ok((Some(low), Some(high))) = self.bounds() else {
            return Vec::new();
        };
        let (Ok(first), Ok(last)) = (low[..4].parse::<u16>(), high[..4].parse::<u16>()) else {
            return Vec::new();
        };
        if last < first || last - first >= 5 {
            return Vec::new();
        }
        (first..=last).collect()
    }

    pub fn bounds(&self) -> Result<(Option<String>, Option<String>), String> {
        let start = self.start.trim();
        let end = self.end.trim();
        let low = (!start.is_empty())
            .then(|| boundary(start, false))
            .transpose()?;
        // One field denotes the whole specified period. An end alone is open-start.
        let high_input = if end.is_empty() { start } else { end };
        let high = (!high_input.is_empty())
            .then(|| boundary(high_input, true))
            .transpose()?;
        if let (Some(low), Some(high)) = (&low, &high)
            && low > high
        {
            return Err("The start must be before the end".into());
        }
        Ok((low, high))
    }

    pub fn contains(&self, added_at: Option<&str>) -> bool {
        let Ok((low, high)) = self.bounds() else {
            return false;
        };
        if low.is_none() && high.is_none() {
            return true;
        }
        let Some(timestamp) = added_at.and_then(|value| value.parse::<jiff::Timestamp>().ok())
        else {
            return false;
        };
        let date = timestamp
            .to_zoned(jiff::tz::TimeZone::UTC)
            .date()
            .to_string();
        low.as_deref().is_none_or(|low| date.as_str() >= low)
            && high.as_deref().is_none_or(|high| date.as_str() <= high)
    }

    pub fn label(&self) -> String {
        let start = self.start.trim().replace('/', "-");
        let end = self.end.trim().replace('/', "-");
        match (start.is_empty(), end.is_empty()) {
            (true, true) => "Todas".into(),
            (false, true) => start,
            (true, false) => format!("até {end}"),
            _ if start == end => start,
            _ => format!("{start} a {end}"),
        }
    }
}
