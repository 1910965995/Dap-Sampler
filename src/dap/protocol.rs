use crate::error::*;
use crate::dap::commands::*;
use crate::usb::transfer::BulkTransfer;

/// DAP 协议处理层
#[derive(Clone, Copy)]
pub struct DapProtocol {
    pub dap_index: u8,
}

impl DapProtocol {
    pub fn new() -> Self {
        Self { dap_index: 0 }
    }
}

impl Default for DapProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl DapProtocol {
    // --------------------------------------------------------
    // DAP_Info
    // --------------------------------------------------------
    pub fn build_info_request(&self, info_id: u8) -> Vec<u8> {
        vec![DAP_INFO, info_id]
    }

    pub fn parse_info_response(data: &[u8]) -> Result<(u8, Vec<u8>)> {
        if data.len() < 2 {
            return Err(Error::InvalidResponse("DAP_Info response too short".into()));
        }
        // CMSIS-DAP 响应格式: [命令回显(0x00), 状态, 内容...]
        let status = data[1];
        let content = if data.len() > 2 { data[2..].to_vec() } else { vec![] };
        Ok((status, content))
    }

    // --------------------------------------------------------
    // DAP_Connect
    // --------------------------------------------------------
    pub fn build_connect_request(&self, mode: u8) -> Vec<u8> {
        vec![DAP_CONNECT, mode]
    }

    pub fn parse_connect_response(data: &[u8]) -> Result<u8> {
        if data.len() < 2 {
            return Err(Error::InvalidResponse("DAP_Connect response too short".into()));
        }
        // CMSIS-DAP 响应格式: [命令回显(0x02), 端口值]
        // 端口值: 0=未连接, 1=SWD, 2=JTAG
        Ok(data[1])
    }

    // --------------------------------------------------------
    // DAP_SWJ_Clock
    // --------------------------------------------------------
    pub fn build_clock_request(&self, freq_hz: u32) -> Vec<u8> {
        let mut cmd = vec![DAP_SWJ_CLOCK];
        cmd.extend_from_slice(&freq_hz.to_le_bytes());
        cmd
    }

    pub fn parse_clock_response(data: &[u8]) -> Result<u8> {
        if data.len() < 2 {
            return Err(Error::InvalidResponse("DAP_SWJ_Clock response too short".into()));
        }
        // CMSIS-DAP 响应格式: [命令回显(0x11), 状态(0=成功)]
        Ok(data[1])
    }

    // --------------------------------------------------------
    // DAP_TransferConfigure (必须在 DAP_Transfer 之前调用)
    // --------------------------------------------------------
    /// 构建 DAP_TransferConfigure 请求
    /// idle_cycles: 空闲周期数
    /// wait_retry: WAIT 重试次数
    /// match_retry: Match 重试次数
    pub fn build_transfer_configure_request(idle_cycles: u8, wait_retry: u16, match_retry: u16) -> Vec<u8> {
        let mut cmd = vec![DAP_TRANSFER_CONFIGURE, idle_cycles];
        cmd.extend_from_slice(&wait_retry.to_le_bytes());
        cmd.extend_from_slice(&match_retry.to_le_bytes());
        cmd
    }

    pub fn parse_transfer_configure_response(data: &[u8]) -> Result<()> {
        if data.len() < 2 {
            return Err(Error::InvalidResponse("DAP_TransferConfigure response too short".into()));
        }
        if data[1] != 0 {
            return Err(Error::InvalidResponse(format!("DAP_TransferConfigure failed: status={}", data[1])));
        }
        Ok(())
    }

    // --------------------------------------------------------
    // DAP_SWD_Configure (配置 SWD 通信参数)
    // --------------------------------------------------------
    /// 构建 DAP_SWD_Configure 请求
    /// cfg: bit[1:0]=turnaround cycles, bit[2]=dataPhase(0=不使用,1=使用)
    pub fn build_swd_configure_request(cfg: u8) -> Vec<u8> {
        vec![DAP_SWD_CONFIGURE, cfg]
    }

    pub fn parse_swd_configure_response(data: &[u8]) -> Result<()> {
        if data.len() < 2 {
            return Err(Error::InvalidResponse("DAP_SWD_Configure response too short".into()));
        }
        if data[1] != 0 {
            return Err(Error::InvalidResponse(format!("DAP_SWD_Configure failed: status={}", data[1])));
        }
        Ok(())
    }

    // --------------------------------------------------------
    // DAP_HostStatus (通知 DAP-Link 主机连接状态)
    // --------------------------------------------------------
    /// 构建 DAP_HostStatus 请求
    /// status_type: 0=Connect, 1=Running
    /// status: 0=Off, 1=On
    pub fn build_host_status_request(status_type: u8, status: u8) -> Vec<u8> {
        vec![DAP_LED, status_type, status]
    }

    pub fn parse_host_status_response(data: &[u8]) -> Result<()> {
        if data.len() < 2 {
            return Err(Error::InvalidResponse("DAP_HostStatus response too short".into()));
        }
        // 忽略状态，某些 DAP-Link 可能不支持
        Ok(())
    }

    // --------------------------------------------------------
    // DAP_SWJ_PINS (控制 nRESET/SWCLK/SWDIO 引脚电平)
    // --------------------------------------------------------
    /// 构建 DAP_SWJ_PINS 请求
    /// output: 引脚输出值（bit0=SWCLK, bit1=SWDIO/TMS, bit2=TDI, bit3=TDO, bit5=nTRST, bit7=nRESET）
    /// select: 要修改的引脚位掩码（同 output 位定义）
    /// wait_us: 设置后等待的微秒数
    pub fn build_pins_request(&self, output: u8, select: u8, wait_us: u32) -> Vec<u8> {
        let mut cmd = vec![DAP_SWJ_PINS, output, select];
        cmd.extend_from_slice(&wait_us.to_le_bytes());
        cmd
    }

    pub fn parse_pins_response(data: &[u8]) -> Result<u8> {
        if data.len() < 2 {
            return Err(Error::InvalidResponse("DAP_SWJ_PINS response too short".into()));
        }
        Ok(data[1]) // 引脚状态
    }

    // --------------------------------------------------------
    // DAP_SWJ_Sequence
    // --------------------------------------------------------
    /// 构建 DAP_SWJ_Sequence 请求
    /// count 是比特数（不是字节数）！data.len() * 8 = 总比特数
    pub fn build_swj_sequence_request(&self, data: &[u8]) -> Vec<u8> {
        let bit_count = (data.len() * 8).min(256) as u8; // 最大 256 位
        let mut cmd = vec![DAP_SWJ_SEQUENCE, bit_count];
        cmd.extend_from_slice(data);
        cmd
    }

    pub fn parse_swj_sequence_response(data: &[u8]) -> Result<u8> {
        if data.len() < 2 {
            return Err(Error::InvalidResponse("DAP_SWJ_Sequence response too short".into()));
        }
        Ok(data[1])
    }

    // --------------------------------------------------------
    // DAP_Transfer
    // --------------------------------------------------------
    /// 构建 DAP_Transfer 命令
    pub fn build_transfer_request(&self, requests: &[TransferRequest]) -> Vec<u8> {
        let mut cmd = vec![DAP_TRANSFER, self.dap_index, requests.len() as u8];

        for req in requests {
            cmd.push(req.request_byte());
            if !req.rnw {
                // 写操作：后跟 4 字节数据（小端序）
                let data = req.write_data.unwrap_or(0);
                cmd.extend_from_slice(&data.to_le_bytes());
            }
        }
        cmd
    }

    /// 解析 DAP_Transfer 响应
    ///
    /// CMSIS-DAP v2 格式: [命令回显(0x05), 计数, 状态, 数据(每成功读操作4字节)...]
    /// `count` 是设备实际执行的请求数——FAULT/WAIT 时序列提前中止，
    /// 计数可能小于发出的请求数，数据区只包含已成功完成的读操作。
    /// 调用方必须传入恰好一条响应（长度按 count 动态计算，见
    /// pipeline::engine::transfer_frame_len），否则会解析到垃圾数据。
    pub fn parse_transfer_response(data: &[u8]) -> Result<TransferResponse> {
        if data.len() < 3 {
            return Err(Error::InvalidResponse(format!(
                "DAP_Transfer response too short: {} bytes", data.len()
            )));
        }
        if data[0] != DAP_TRANSFER {
            return Err(Error::InvalidResponse(format!(
                "response header is not a DAP_Transfer echo: 0x{:02X} (byte stream desync)",
                data[0]
            )));
        }

        let count = data[1];   // 处理的请求数
        let status = data[2];  // 执行状态

        // 读返回数据：从 byte 3 开始，每 4 字节一个 u32
        // 只解析 data 中实际存在的完整 u32（调用方已按响应长度切片）
        // 定长数组写入：采样热路径零堆分配
        let data_bytes = &data[3..];
        let capacity = crate::pipeline::sample::MAX_VALUES * 3;
        let num_values = (data_bytes.len() / 4).min(capacity);
        let mut values = [0u32; crate::pipeline::sample::MAX_VALUES * 3];

        for (i, slot) in values.iter_mut().enumerate().take(num_values) {
            let offset = i * 4;
            let bytes: [u8; 4] = data_bytes[offset..offset + 4]
                .try_into()
                .map_err(|_| Error::InvalidResponse("data parse error".into()))?;
            *slot = u32::from_le_bytes(bytes);
        }

        Ok(TransferResponse { status, count, data: values, len: num_values })
    }

    /// 便捷方法：执行一次 DAP_Transfer
    pub fn execute_transfer(
        &self,
        usb: &BulkTransfer,
        requests: &[TransferRequest],
    ) -> Result<TransferResponse> {
        let cmd = self.build_transfer_request(requests);
        log::debug!(">>> DAP_Transfer request ({} bytes): {:02X?}", cmd.len(), cmd);
        usb.write(&cmd)?;
        let mut buf = vec![0u8; 1024];
        let n = usb.read(&mut buf)?;
        log::debug!("<<< DAP_Transfer response ({} bytes): {:02X?}", n, &buf[..n]);
        DapProtocol::parse_transfer_response(&buf[..n])
    }

    // --------------------------------------------------------
    // 复合操作：查询 DAP 信息
    // --------------------------------------------------------
    /// 查询 DAP-Link 设备信息
    pub fn query_info(&self, usb: &BulkTransfer) -> Result<DapInfo> {
        let vendor = self.query_info_string(usb, INFO_ID_VENDOR)?;
        let product = self.query_info_string(usb, INFO_ID_PRODUCT)?;
        let serial = self.query_info_string(usb, INFO_ID_SER_NUM)?;
        let fw_version = self.query_info_string(usb, INFO_ID_FW_VER)?;

        let packet_size = self.query_info_u16(usb, INFO_ID_PACKET_SIZE)?;
        let packet_count = self.query_info_u8(usb, INFO_ID_PACKET_COUNT)?;

        Ok(DapInfo { vendor, product, serial, fw_version, packet_size, packet_count })
    }

    fn query_info_raw(&self, usb: &BulkTransfer, id: u8) -> Result<(u8, Vec<u8>)> {
        let cmd = self.build_info_request(id);
        usb.write(&cmd)?;
        let mut buf = [0u8; 64];
        let n = usb.read(&mut buf)?;
        Self::parse_info_response(&buf[..n])
    }

    fn query_info_string(&self, usb: &BulkTransfer, id: u8) -> Result<String> {
        let (_, content) = self.query_info_raw(usb, id)?;
        let s = String::from_utf8_lossy(&content).trim_end_matches('\0').to_string();
        Ok(s)
    }

    fn query_info_u16(&self, usb: &BulkTransfer, id: u8) -> Result<u16> {
        let (_, content) = self.query_info_raw(usb, id)?;
        if content.len() >= 2 {
            Ok(u16::from_le_bytes([content[0], content[1]]))
        } else {
            Ok(0)
        }
    }

    fn query_info_u8(&self, usb: &BulkTransfer, id: u8) -> Result<u8> {
        let (_, content) = self.query_info_raw(usb, id)?;
        Ok(content.first().copied().unwrap_or(0))
    }
}
