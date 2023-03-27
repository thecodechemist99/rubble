//! Integrated 2.4 GHz radio with BLE support.
//!
//! The radio can be used with Nordic's own proprietary protocol, which I don't really care about,
//! so this will focus on Bluetooth Low-Energy (BLE).
//!
//! The radio can calculate the CRC, perform data whitening, automatically send the right preamble,
//! and match addresses.
//!
//! In order to be able to receive packet at all, the length of the currently received packet must
//! be known.
//!
//! # CRC
//!
//! To be able to correctly compute the CRC, the position and length of the transmitted PDU must be
//! known. The radio works with a flexible frame layout that looks like this on air:
//!
//! (B = Byte = Octet; b = bit = symbol)
//!
//! If field length is specified in `B`, only whole Bytes are allowed. If the length is specified
//! in `b`, any number of bits in the given range is allowed.
//!
//! ```notrust
//! +----------+---------+--------+---------+----------+----------+--------------+---------+
//! | Preamble |  Base   | Prefix |   S0    |  Length  |    S1    |   Payload    |   CRC   |
//! |  (1 B)   | (2-4 B) | (1 B)  | (0-1 B) | (0-15 b) | (0-15 b) | (`Length` B) | (0-3 B) |
//! +----------+---------+--------+---------+----------+----------+--------------+---------+
//!             \                / \                                            /
//!              \------+-------/   \---------------------+--------------------/
//!                     |                                 |
//!                  Address                             PDU
//! ```
//!
//! If S0, Length, and S1 are present (= length > 0 bits), their sizes are *always* rounded up to
//! whole Bytes in RAM. The least significant bits of the in-RAM Bytes will be sent/received over
//! the air. This poses a problem, since the packet isn't actually sent as it is in memory. The
//! stack works around this by only filling the `Payload` by itself and passing a `Header` struct to
//! the `Transmitter`, which can then do whatever is necessary to encode the header so that it's
//! sent correctly.
//!
//! In our case, this involves "splitting" the header into the `S0` field (everything preceding the
//! length), the `Length` field, and the `S1` field (which just contains 2 unused bits, but they
//! must still be sent, of course).

use crate::pac;
use crate::pac::{radio::state::STATE_R, RADIO};
use core::cmp;
use core::sync::atomic::{compiler_fence, Ordering};
use rubble::config::Config;
use rubble::link::{
    advertising, data, Cmd, LinkLayer, RadioCmd, Transmitter, CRC_POLY, MIN_PDU_BUF,
};
use rubble::phy::{AdvertisingChannel, DataChannel};
use rubble::time::{Duration, Instant, T_IFS};

/// A packet buffer that can hold header and payload of any advertising or data channel packet.
pub type PacketBuffer = [u8; MIN_PDU_BUF];

/// An interface to the nRF radio in BLE mode.
pub struct BleRadio {
    /// `true` if the radio is operating on an advertising channel, `false` if it's a data channel.
    advertising: bool,
    radio: RADIO,
    tx_buf: &'static mut PacketBuffer,

    /// Receive buffer.
    ///
    /// This is an `Option` because we need to pass a `&mut BleRadio` to the BLE stack while still
    /// having access to this buffer.
    rx_buf: Option<&'static mut PacketBuffer>,
}

impl BleRadio {
    /// Initializes the radio in BLE mode and takes ownership of the RX and TX buffers.
    // TODO: Use type-safe clock configuration to ensure that chip uses ext. crystal
    pub fn new(
        radio: RADIO,
        ficr: &pac::FICR,
        tx_buf: &'static mut PacketBuffer,
        rx_buf: &'static mut PacketBuffer,
    ) -> Self {
        assert!(radio.state.read().state().is_disabled());

        // The nRF51 requires manually setting the trim values.
        #[cfg(feature = "51")]
        {
            if ficr.overrideen.read().ble_1mbit().is_override() {
                unsafe {
                    radio
                        .override0
                        .write(|w| w.override0().bits(ficr.ble_1mbit[0].read().bits()));
                    radio
                        .override1
                        .write(|w| w.override1().bits(ficr.ble_1mbit[1].read().bits()));
                    radio
                        .override2
                        .write(|w| w.override2().bits(ficr.ble_1mbit[2].read().bits()));
                    radio
                        .override3
                        .write(|w| w.override3().bits(ficr.ble_1mbit[3].read().bits()));
                    radio.override4.write(|w| {
                        w.override4()
                            .bits(ficr.ble_1mbit[4].read().bits())
                            .enable()
                            .set_bit()
                    });
                }
            }
        }

        // The nRF52/53 do not require setting trim values, but we take the `ficr` reference anyways
        // to have a consistent interface. Silence the unused variable warning:
        let _ = ficr;

        radio.mode.write(|w| w.mode().ble_1mbit());
        radio.txpower.write(|w| w.txpower().pos4d_bm());

        let max_payload = rx_buf.len() - 2;
        assert!(max_payload <= usize::from(u8::max_value()));

        unsafe {
            radio.pcnf1.write(|w| {
                // no packet length limit
                w.maxlen()
                    .bits(max_payload as u8)
                    // 3-Byte Base Address + 1-Byte Address Prefix
                    .balen()
                    .bits(3)
                    // Enable Data Whitening over PDU+CRC
                    .whiteen()
                    .set_bit()
            });

            radio.crccnf.write(|w| {
                // skip address since only the S0, Length, S1 and Payload need CRC
                // 3 Bytes = CRC24
                w.skipaddr().skip().len().three()
            });

            radio
                .crcpoly
                .write(|w| w.crcpoly().bits(CRC_POLY & 0x00FFFFFF));

            // Configure logical address 0 as the canonical advertising address.
            // Base addresses are up to 32 bits in size. However, an 8 bit Address Prefix is
            // *always* appended, so we must use a 24 bit Base Address and the 8 bit Prefix.
            // BASE0 has, apparently, undocumented semantics: It is a proper 32-bit register, but
            // it ignores the *lowest* 8 bit and instead transmits the upper 24 as the low 24 bits
            // of the Access Address. Shift address up to fix this.
            radio
                .base0
                .write(|w| w.bits(advertising::ACCESS_ADDRESS << 8));
            radio
                .prefix0
                .write(|w| w.ap0().bits((advertising::ACCESS_ADDRESS >> 24) as u8));
        }

        // FIXME: No TIFS hardware support for now. Revisit when precise semantics are clear.
        // Activate END_DISABLE and DISABLED_TXEN shortcuts so TIFS is enforced. We might enable
        // more shortcuts later.
        /*radio.shorts.write(|w| w
            .end_disable().enabled()
            .disabled_txen().enabled()
        );*/

        /*unsafe {
            radio.tifs.write(|w| w.tifs().bits(BLE_TIFS));
        }*/

        // Configure shortcuts to simplify and speed up sending and receiving packets.
        radio.shorts.write(|w| {
            // start transmission/recv immediately after ramp-up
            // disable radio when transmission/recv is done
            w.ready_start().enabled().end_disable().enabled()
        });

        // We can now start the TXEN/RXEN tasks and the radio will do the rest and return to the
        // disabled state.

        Self {
            advertising: false,
            radio,
            tx_buf,
            rx_buf: Some(rx_buf),
        }
    }

    /// Returns the current radio state.
    pub fn state(&self) -> STATE_R {
        self.radio.state.read().state()
    }

    /// Configures the Radio for (not) receiving data according to `cmd`.
    pub fn configure_receiver(&mut self, cmd: RadioCmd) {
        // Waits for the end of any ongoing transmissions. Don't wait if we lost the last connection
        // event, since we shouldn't be transmitting anyway
        if let RadioCmd::ListenData { timeout, .. } = cmd {
            if !timeout {
                while self.state().is_tx() || self.state().is_tx_ru() {}
            }
        }
        // "Subsequent reads and writes cannot be moved ahead of preceding reads."
        compiler_fence(Ordering::Acquire);

        // Disable `DISABLED` interrupt, effectively stopping reception
        self.radio.intenclr.write(|w| w.disabled().clear());

        // Acknowledge left-over disable event
        self.radio.events_disabled.reset();
        // Disable radio
        self.radio.tasks_disable.write(|w| unsafe { w.bits(1) });
        // Then wait until disable event is triggered
        while self.radio.events_disabled.read().bits() == 0 {}
        // And acknowledge it
        self.radio.events_disabled.reset();

        match cmd {
            RadioCmd::Off => {}
            RadioCmd::ListenAdvertising { channel } => {
                self.prepare_txrx_advertising(channel);

                let rx_buf = (*self.rx_buf.as_mut().unwrap()) as *mut _ as u32;
                self.radio.packetptr.write(|w| unsafe { w.bits(rx_buf) });

                // Enable `DISABLED` interrupt (packet fully received)
                self.radio.intenset.write(|w| w.disabled().set());

                // Match on logical address 0 only
                self.radio.rxaddresses.write(|w| w.addr0().enabled());

                // Enable the correct shortcuts in case it was changed in a previous connection.
                self.radio
                    .shorts
                    .write(|w| w.ready_start().enabled().end_disable().enabled());

                // "Preceding reads and writes cannot be moved past subsequent writes."
                compiler_fence(Ordering::Release);

                // ...and enter RX mode
                self.radio.tasks_rxen.write(|w| unsafe { w.bits(1) });
            }
            RadioCmd::ListenData {
                channel,
                access_address,
                crc_init,
                ..
            } => {
                self.prepare_txrx_data(channel, access_address, crc_init);

                // Enforce T_IFS in hardware.
                self.radio
                    .tifs
                    .write(|w| unsafe { w.bits(T_IFS.to_micros()) });

                let rx_buf = (*self.rx_buf.as_mut().unwrap()) as *mut _ as u32;
                self.radio.packetptr.write(|w| unsafe { w.bits(rx_buf) });

                // Enable `DISABLED` interrupt (packet fully received)
                self.radio.intenset.write(|w| w.disabled().set());

                // Match on logical address 1 only
                self.radio.rxaddresses.write(|w| w.addr1().enabled());

                // "Preceding reads and writes cannot be moved past subsequent writes."
                compiler_fence(Ordering::Release);

                // ...and enter RX mode
                self.radio.tasks_rxen.write(|w| unsafe { w.bits(1) });

                // Enable the required shortcuts for T_IFS. The radio will go into `TXIDLE` state
                // automatically after receiving a packet.
                self.radio.shorts.write(|w| {
                    w.end_disable()
                        .enabled()
                        .disabled_txen()
                        .enabled()
                        .ready_start()
                        .enabled()
                });
            }
        }
    }

    /// Call this when the `RADIO` interrupt fires.
    ///
    /// Automatically reconfigures the radio according to the `RadioCmd` returned by the BLE stack.
    ///
    /// Returns when the `update` method should be called the next time.
    pub fn recv_interrupt<C: Config<Transmitter = Self>>(
        &mut self,
        timestamp: Instant,
        ll: &mut LinkLayer<C>,
    ) -> Option<Cmd> {
        if self.radio.events_disabled.read().bits() == 0 {
            return None;
        }

        // "Subsequent reads and writes cannot be moved ahead of preceding reads."
        compiler_fence(Ordering::Acquire);

        // Acknowledge DISABLED event:
        self.radio.events_disabled.reset();

        let crc_ok = self.radio.crcstatus.read().crcstatus().is_crcok();

        let cmd = if self.advertising {
            // When we get here, the radio must have transitioned to DISABLED state.
            assert!(self.state().is_disabled());

            let header = advertising::Header::parse(*self.rx_buf.as_ref().unwrap());

            // check that `payload_length` is in bounds
            let rx_buf = self.rx_buf.take().unwrap();
            let pl_lim = cmp::min(2 + usize::from(header.payload_length()), rx_buf.len());
            let payload = &rx_buf[2..pl_lim];
            let cmd = ll.process_adv_packet(timestamp, self, header, payload, crc_ok);
            self.rx_buf = Some(rx_buf);
            cmd
        } else {
            // Important! Turn ready->start off before TXREADY is reached (in ~150µs)
            self.radio.shorts.modify(|_, w| w.ready_start().disabled());
            assert!(!self.state().is_tx());

            let header = data::Header::parse(*self.rx_buf.as_ref().unwrap());

            // check that `payload_length` is in bounds
            let rx_buf = self.rx_buf.take().unwrap();
            let pl_lim = cmp::min(2 + usize::from(header.payload_length()), rx_buf.len());
            let payload = &rx_buf[2..pl_lim];
            let cmd = ll.process_data_packet(timestamp, self, header, payload, crc_ok);
            self.rx_buf = Some(rx_buf);
            cmd
        };

        Some(cmd)
    }

    /// Perform preparations to receive or send on an advertising channel.
    ///
    /// This will disable the radio, configure the packet layout, set initial values for CRC and
    /// whitening, and set the frequency to the given `channel`.
    ///
    /// To **transmit**, the `txaddress` must be set and the `packetptr` must be set to the TX
    /// buffer.
    ///
    /// To **receive**, the `rxaddresses` must be set to receive on logical address 0 and
    /// `packetptr` must be pointed to the RX buffer.
    ///
    /// Of course, other tasks may also be performed.
    fn prepare_txrx_advertising(&mut self, channel: AdvertisingChannel) {
        self.advertising = true;

        unsafe {
            // Acknowledge left-over disable event
            self.radio.events_disabled.reset();

            if !self.state().is_disabled() {
                // In case we're currently receiving, stop that
                self.radio.tasks_disable.write(|w| w.bits(1));

                // Then wait until disable event is triggered
                while self.radio.events_disabled.read().bits() == 0 {}
            }
        }

        assert!(self.state().is_disabled());

        // Now we can freely configure all registers we need
        unsafe {
            self.radio
                .pcnf0
                .write(|w| w.s0len().bit(true).lflen().bits(8).s1len().bits(0));

            self.radio
                .datawhiteiv
                .write(|w| w.datawhiteiv().bits(channel.whitening_iv()));
            self.radio
                .crcinit
                .write(|w| w.crcinit().bits(advertising::CRC_PRESET));
            self.radio
                .frequency
                .write(|w| w.frequency().bits((channel.freq() - 2400) as u8));
        }
    }

    fn prepare_txrx_data(&mut self, channel: DataChannel, access_address: u32, crc_init: u32) {
        self.advertising = false;

        unsafe {
            self.radio
                .pcnf0
                .write(|w| w.s0len().bit(true).lflen().bits(8).s1len().bits(0));

            self.radio
                .datawhiteiv
                .write(|w| w.datawhiteiv().bits(channel.whitening_iv()));
            self.radio
                .crcinit
                .write(|w| w.crcinit().bits(crc_init & 0x00FFFFFF));
            self.radio
                .frequency
                .write(|w| w.frequency().bits((channel.freq() - 2400) as u8));

            // Address #1 is our data channel access address
            self.radio.base1.write(|w| w.bits(access_address << 8));
            self.radio
                .prefix0
                .write(|w| w.ap1().bits((access_address >> 24) as u8));
        }
    }

    /// Transmit a PDU from the internal buffer.
    ///
    /// This will block until the transmission has completed.
    ///
    /// Assumes that all registers are correct for this type of transmission.
    fn transmit(&mut self) {
        assert!(self.state().is_disabled());

        unsafe {
            // "The CPU should reconfigure this pointer every time before the RADIO is started via
            // the START task."
            self.radio
                .packetptr
                .write(|w| w.bits(self.tx_buf as *const _ as u32));

            // Acknowledge left-over disable event
            self.radio.events_disabled.reset(); // FIXME unnecessary, right?

            // "Preceding reads and writes cannot be moved past subsequent writes."
            compiler_fence(Ordering::Release);

            // ...and kick off the transmission
            self.radio.tasks_txen.write(|w| w.bits(1));

            // Then wait until disable event is triggered
            while self.radio.events_disabled.read().bits() == 0 {}

            // "Subsequent reads and writes cannot be moved ahead of preceding reads."
            compiler_fence(Ordering::Acquire);

            // Now our `tx_buf` can be used again.
        }
    }
}

impl Transmitter for BleRadio {
    fn tx_payload_buf(&mut self) -> &mut [u8] {
        // Wait for any ongoing transmissions
        while self.state().is_tx() {}
        // "Subsequent reads and writes cannot be moved ahead of preceding reads."
        compiler_fence(Ordering::Acquire);

        // Leave 2 Bytes for the data/advertising PDU header.
        &mut self.tx_buf[2..]
    }

    fn transmit_advertising(&mut self, header: advertising::Header, channel: AdvertisingChannel) {
        let raw_header = header.to_u16();
        // S0 = 8 bits (LSB)
        self.tx_buf[0] = raw_header as u8;
        // Length = 6 bits, followed by 2 RFU bits (0)
        self.tx_buf[1] = header.payload_length();

        self.prepare_txrx_advertising(channel);

        // Set transmission address:
        // Logical addr. 0 uses BASE0 + PREFIX0, which is the canonical adv. Access Address
        self.radio
            .txaddress
            .write(|w| unsafe { w.txaddress().bits(0) });

        self.transmit();
    }

    fn transmit_data(
        &mut self,
        _access_address: u32,
        _crc_iv: u32,
        header: data::Header,
        _channel: DataChannel,
    ) {
        let raw_header = header.to_u16();
        // S0 = 8 bits (LSB)
        self.tx_buf[0] = raw_header as u8;
        // Length = 8 bits (or fewer, for BT versions <4.2)
        self.tx_buf[1] = header.payload_length();

        // Set transmission address:
        // Logical addr. 1 uses BASE1 + PREFIX1, which is set to the data channel address
        self.radio
            .txaddress
            .write(|w| unsafe { w.txaddress().bits(1) });

        // "The CPU should reconfigure this pointer every time before the RADIO is started via
        // the START task."
        self.radio
            .packetptr
            .write(|w| unsafe { w.bits(self.tx_buf as *const _ as u32) });

        // "Preceding reads and writes cannot be moved past subsequent writes."
        compiler_fence(Ordering::Release);

        // ...and kick off the transmission
        self.radio
            .shorts
            .write(|w| w.ready_start().enabled().end_disable().disabled());
    }
}
