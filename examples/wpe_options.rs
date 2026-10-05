//! A `<select>` opens a list the chrome can draw and answer, end to end.
//!
//! WPE has no popup of its own, so a select only opens if the embedder
//! handles `show-option-menu`. Against the real engine, at output scale 2:
//!
//! * a click on the select hands over its options — labels, the optgroup
//!   heading and child flags, the disabled one, the current value — and its
//!   box in *logical* pixels, the chrome's own;
//! * a pick changes the value and fires `change`, and closes the list;
//! * closing without a pick leaves the value alone and fires nothing;
//! * a navigation while the list is open closes it from the page's side.
//!
//! `cce-shadow --instance <n> run ./target/release/examples/wpe_options`

#[cfg(not(feature = "wpe"))]
fn main() {
    eprintln!("build with --features wpe");
}

#[cfg(feature = "wpe")]
#[derive(Debug, Clone, Copy)]
pub enum EditingCommand { Copy, Cut, Paste }

#[cfg(feature = "wpe")]
#[path = "../src/pages.rs"]
mod pages;
#[cfg(feature = "wpe")]
#[path = "../src/downloads.rs"]
mod downloads;
#[cfg(feature = "wpe")]
#[path = "../src/wpe/mod.rs"]
mod wpe;

#[cfg(feature = "wpe")]
fn main() {
    use cce_ui::widget::MouseButton;
    let page = "data:text/html,<!doctype html><title>start</title>\
<style>body{margin:0}select{position:absolute;left:100px;top:200px;width:150px;height:30px}</style>\
<select id=q onchange=\"document.title='change:'+this.value\">\
<option>1</option><option selected>2</option><option disabled>3</option>\
<optgroup label=More><option>4</option><option>5</option></optgroup></select>";
    let mut host = wpe::WebKitHost::new(url::Url::parse(page).unwrap(), (2400, 1600));
    host.resize(2400, 1600, 2.0);
    let settle = |h: &mut wpe::WebKitHost, n: u32| {
        for _ in 0..n {
            h.pump();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    // Physical pixels, as main.rs hands them over: the select's middle is
    // (175, 215) logical.
    let click = |h: &mut wpe::WebKitHost| {
        let (x, y) = (175.0 * 2.0, 215.0 * 2.0);
        h.mouse_move(x, y);
        h.mouse_button_ui(MouseButton::Left, true, x, y);
        h.mouse_button_ui(MouseButton::Left, false, x, y);
    };
    settle(&mut host, 30);
    println!("loaded: {:?}", host.title());
    let mut ok = true;
    let mut check = |name: &str, pass: bool| {
        println!("  {name:<40} {}", if pass { "OK" } else { "FAIL" });
        ok &= pass;
    };

    click(&mut host);
    settle(&mut host, 10);
    let menu = host.take_option_menu();
    println!("menu: {menu:#?}");
    check("click opens the list", menu.is_some() && host.option_menu_open());
    if let Some(m) = &menu {
        let labels: Vec<_> = m.items.iter().map(|i| i.label.as_str()).collect();
        check("  labels in order", labels == ["1", "2", "3", "More", "4", "5"]);
        check("  current value marked", m.items[1].selected && !m.items[0].selected);
        check("  disabled option marked", !m.items[2].enabled);
        check("  optgroup heading marked", m.items[3].group_label);
        check("  group children marked", m.items[4].group_child && m.items[5].group_child);
        let (x, y, w, h) = m.anchor;
        check(
            "  anchor in logical pixels",
            (x - 100.0).abs() <= 1.0 && (y - 200.0).abs() <= 1.0 && (w - 150.0).abs() <= 1.0 && (h - 30.0).abs() <= 1.0,
        );
    }

    host.pick_option(4);
    settle(&mut host, 10);
    println!("after pick: {:?}", host.title());
    check("pick changes the value", host.title().as_deref() == Some("change:4"));
    check("  and closes the list", !host.option_menu_open());

    click(&mut host);
    settle(&mut host, 10);
    check("opens again", host.take_option_menu().is_some());
    host.close_option_menu();
    settle(&mut host, 10);
    check("close leaves the value", host.title().as_deref() == Some("change:4"));
    check("  and closes the list", !host.option_menu_open());

    click(&mut host);
    settle(&mut host, 10);
    check("opens a third time", host.take_option_menu().is_some());
    host.load(url::Url::parse("data:text/html,<title>away</title>").unwrap());
    settle(&mut host, 20);
    println!("after navigation: {:?}", host.title());
    check("navigation closes it from the page", !host.option_menu_open());

    println!("\n{}", if ok { "OK" } else { "FAILED" });
    std::process::exit(if ok { 0 } else { 1 });
}
