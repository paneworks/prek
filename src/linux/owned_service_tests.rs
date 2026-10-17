use super::*;

struct EarlyStop {
    serviced: bool,
    headers: usize,
    config: crate::config::Config,
}
impl InputSink for EarlyStop {
    fn push(&mut self, record: Record) -> io::Result<()> {
        assert!(matches!(record, Record::Header { .. }));
        self.headers += 1;
        Ok(())
    }
    fn deadline(&self) -> Option<Duration> {
        None
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn service(&mut self) -> io::Result<()> {
        panic!("reader loop bypassed the owned service hook");
    }
    fn service_owned(&mut self, health: Option<&crate::supervisor::Heartbeat>) -> io::Result<()> {
        assert!(health.is_none());
        self.serviced = true;
        Ok(())
    }
    fn finished(&self) -> bool {
        self.serviced
    }
    fn device_policy(&self) -> Option<(&crate::config::Config, u64)> {
        Some((&self.config, 1))
    }
}

#[test]
fn an_empty_reader_loop_services_the_owned_hook_before_discovery_or_stop() {
    let mut sink = EarlyStop {
        serviced: false,
        headers: 0,
        config: crate::config::Config::preset("minimal").unwrap(),
    };
    observe(&[], Some(Duration::ZERO), &mut sink).unwrap();
    assert!(sink.serviced);
    assert_eq!(sink.headers, 1);
}
