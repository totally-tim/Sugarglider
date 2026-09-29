// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reads the native window tab bar, without selecting a tab.

use accessibility::{
    AXAttribute, AXAttributeValue, AXError, AXUIElement, AXUIElementAttributes, Error,
};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};

pub struct NativeTabBar {
    pub element: CFRetained<AXUIElement>,
    pub tabs: Vec<CFRetained<AXUIElement>>,
    pub selected: usize,
}

/// Only a direct window child with native tab-button contents qualifies.
/// Missing attributes on a previously identified bar remain an error.
pub fn read(
    window: &AXUIElement,
    known: Option<&AXUIElement>,
) -> Result<Option<NativeTabBar>, Error> {
    let Some(children) = qualification(window.children(), known.is_some())? else {
        return Ok(None);
    };
    if children.len() > 256 {
        return Err(Error::NotFound);
    }
    let mut result = None;
    for child in children.iter() {
        let known = known == Some(&*child);
        let Some(candidate) = qualification(read_buttons(&child), known)? else {
            continue;
        };
        let Some(tabs) = candidate else {
            if known {
                return Err(Error::NotFound);
            }
            continue;
        };
        if &*child.window()? != window || result.is_some() {
            return Err(Error::NotFound);
        }
        let selected_element = AXUIElement::downcast(child.value()?)?;
        let selected =
            tabs.iter().position(|tab| *tab == selected_element).ok_or(Error::NotFound)?;
        if &*child.window()? != window || AXUIElement::downcast(child.value()?)? != selected_element
        {
            return Err(Error::NotFound);
        }
        result = Some(NativeTabBar { element: child, tabs, selected });
    }
    Ok(result)
}

/// Unsupported attributes describe a different control. A transient read
/// failure, or lost attributes on a known bar, cannot establish that fact.
fn qualification<T>(value: Result<T, Error>, known: bool) -> Result<Option<T>, Error> {
    match value {
        Err(
            Error::Ax(AXError::AttributeUnsupported | AXError::NoValue)
            | Error::UnexpectedType { .. },
        ) if !known => Ok(None),
        value => value.map(Some),
    }
}

fn read_buttons(child: &AXUIElement) -> Result<Option<Vec<CFRetained<AXUIElement>>>, Error> {
    if child.role()?.to_string() != "AXTabGroup" {
        return Ok(None);
    }
    let value = child.attribute(&AXAttribute::new(&CFString::from_static_str("AXTabs")))?;
    let array = CFArray::<CFType>::downcast(value)?;
    if array.is_empty() {
        return Err(Error::Ax(AXError::NoValue));
    }
    if array.len() > 256 {
        return Err(Error::NotFound);
    }
    let tabs: Vec<_> = array.iter().map(AXUIElement::downcast).collect::<Result<_, _>>()?;
    for tab in &tabs {
        if tab.subrole()?.to_string() != "AXTabButton" || &*tab.parent()? != child {
            return Ok(None);
        }
    }
    // Native window bars expose their buttons as AXContents. NSTabView
    // exposes the selected page, even though the tab roles are the same.
    let contents = child.attribute(&AXAttribute::new(&CFString::from_static_str("AXContents")))?;
    let contents = CFArray::<CFType>::downcast(contents)?;
    if contents.len() > 256 {
        return Err(Error::NotFound);
    }
    let contents: Vec<_> = contents.iter().map(AXUIElement::downcast).collect::<Result<_, _>>()?;
    Ok(tabs.iter().all(|tab| contents.contains(tab)).then_some(tabs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_tab_qualification_distinguishes_unsupported_controls_from_failed_reads() {
        for error in [
            Error::Ax(AXError::AttributeUnsupported),
            Error::Ax(AXError::NoValue),
            Error::UnexpectedType { expected: 1, received: 2 },
        ] {
            assert!(matches!(qualification::<()>(Err(error), false), Ok(None)));
        }
        for known in [false, true] {
            for error in [
                AXError::CannotComplete,
                AXError::InvalidUIElement,
                AXError::Failure,
            ] {
                assert!(qualification::<()>(Err(Error::Ax(error)), known).is_err());
            }
        }
        for error in [AXError::AttributeUnsupported, AXError::NoValue] {
            assert!(qualification::<()>(Err(Error::Ax(error)), true).is_err());
        }
    }
}
