/// DAVE channel id of a Go Live stream: `rtc_server_id - 1`, as `StreamConnection.daveChannelId` in JS.
pub fn dave_channel_id(rtc_server_id: &str) -> Option<u64> {
    rtc_server_id.parse::<u64>().ok()?.checked_sub(1)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dave_channel_id_is_server_id_minus_one() {
        assert_eq!(
            super::dave_channel_id("1234567890123456789"),
            Some(1234567890123456788)
        );
        assert_eq!(super::dave_channel_id("0"), None);
        assert_eq!(super::dave_channel_id("x"), None);
    }
}
