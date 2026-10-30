fn main() {
    if komo_channel::login_requested() {
        komo_channel::wechat::login_main();
    } else {
        komo_channel::main(komo_channel::wechat::WeChatFactory::with_alerts(
            komo_channel::alerts(),
        ));
    }
}
