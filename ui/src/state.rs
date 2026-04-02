use delta_core::{Page, PageId, SiteState};
use dioxus::prelude::*;
use ed25519_dalek::Signature;

/// Global site state signal.
pub static SITE: GlobalSignal<SiteState> = GlobalSignal::new(SiteState::default);

/// Currently selected page ID.
pub static CURRENT_PAGE: GlobalSignal<Option<PageId>> = GlobalSignal::new(|| None);

/// Whether we're in edit mode.
pub static EDITING: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// Editor content (buffered separately from saved state).
pub static EDITOR_TITLE: GlobalSignal<String> = GlobalSignal::new(String::new);
pub static EDITOR_CONTENT: GlobalSignal<String> = GlobalSignal::new(String::new);

/// Initialize with example data.
pub fn init_example_data() {
    let data = crate::example_data::create_example_site();
    *SITE.write() = data;
    // Select the first page
    let first_id = SITE.read().pages.keys().next().copied();
    *CURRENT_PAGE.write() = first_id;
}

/// Get the current page (if any).
pub fn current_page() -> Option<(PageId, Page)> {
    let site = SITE.read();
    let id = (*CURRENT_PAGE.read())?;
    site.pages.get(&id).map(|p| (id, p.clone()))
}

/// Create a new page with placeholder content.
pub fn create_page(title: String) -> PageId {
    let mut site = SITE.write();
    let id = site.next_page_id;
    let page = Page {
        title,
        content: String::new(),
        updated_at: now_secs(),
        // Placeholder signature for example mode
        signature: Signature::from_bytes(&[0u8; 64]),
    };
    site.pages.insert(id, page);
    site.next_page_id = id + 1;
    *CURRENT_PAGE.write() = Some(id);
    *EDITING.write() = true;
    id
}

/// Save the current editor contents to the page.
pub fn save_current_page() {
    let Some(id) = *CURRENT_PAGE.read() else {
        return;
    };
    let title = EDITOR_TITLE.read().clone();
    let content = EDITOR_CONTENT.read().clone();

    let mut site = SITE.write();
    if let Some(page) = site.pages.get_mut(&id) {
        page.title = title;
        page.content = content;
        page.updated_at = now_secs();
    }
    *EDITING.write() = false;
}

/// Delete a page by ID.
pub fn delete_page(id: PageId) {
    let mut site = SITE.write();
    site.pages.remove(&id);
    if *CURRENT_PAGE.read() == Some(id) {
        *CURRENT_PAGE.write() = site.pages.keys().next().copied();
    }
}

/// Start editing the current page.
pub fn start_editing() {
    if let Some((_, page)) = current_page() {
        *EDITOR_TITLE.write() = page.title.clone();
        *EDITOR_CONTENT.write() = page.content.clone();
        *EDITING.write() = true;
    }
}

/// Select a page by ID.
pub fn select_page(id: PageId) {
    *EDITING.write() = false;
    *CURRENT_PAGE.write() = Some(id);
}

fn now_secs() -> u64 {
    chrono::Utc::now().timestamp() as u64
}
