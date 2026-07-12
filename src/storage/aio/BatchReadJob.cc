#include "storage/aio/BatchReadJob.h"

#include "common/monitor/Recorder.h"
#include "common/utils/Duration.h"
#include "storage/store/StorageTarget.h"

namespace hf3fs::storage {

monitor::CountRecorder rdmaWriteCount{"storage.rdma_write.count"};
monitor::CountRecorder rdmaWriteFails{"storage.rdma_write.fails"};
monitor::CountRecorder rdmaWriteBytes{"storage.rdma_write.bytes"};
monitor::LatencyRecorder batchReadLatency{"storage.aio.batch_latency"};

monitor::CountRecorder aioChecksumMismatch{"storage.aio.checksum_mismatch"};

AioReadJob::AioReadJob(const ReadIO &readIO, IOResult &result, BatchReadJob &batch)
    : readIO_(readIO),
      result_(result),
      batch_(batch) {
  state_.headLength = readIO_.offset % kAIOAlignSize;
  state_.tailLength = (kAIOAlignSize - (readIO_.offset + readIO_.length) % kAIOAlignSize) % kAIOAlignSize;
}

void AioReadJob::setResult(Result<uint32_t> lengthInfo) {
  if (lengthInfo) {
    auto checksumType = batch_.checksumType();

    if (checksumType == ChecksumType::NONE) {
      result_.checksum = {ChecksumType::NONE, 0U};  // do not return checksum
    } else if (checksumType == state_.chunkChecksum.type && readIO_.offset == 0 && *lengthInfo == state_.chunkLen) {
      result_.checksum = state_.chunkChecksum;  // use chunk checksum if the full chunk is read
    } else {                                    // calculate checksum of the read data
      auto dataBuf = state_.localbuf.subrange(state_.headLength, *lengthInfo);
      result_.checksum = ChecksumInfo::create(checksumType, dataBuf.ptr(), dataBuf.size());
    }

    // check chunk version.
    auto result = state_.storageTarget->aioFinishRead(*this);
    if (UNLIKELY(!result)) {
      lengthInfo = makeError(std::move(result.error()));
    }

    if (batch_.recalculateChecksum() && readIO_.offset == 0 && *lengthInfo == state_.chunkLen) {
      auto realChecksum = ChecksumInfo::create(state_.chunkChecksum.type, state_.localbuf.ptr(), *lengthInfo);
      if (UNLIKELY(realChecksum != state_.chunkChecksum)) {
        aioChecksumMismatch.addSample(1);
        auto msg = fmt::format("aio checksum mismatch, read: {}, state: {}, checksum: {}",
                               readIO(),
                               state(),
                               realChecksum.value);
        XLOG(CRITICAL, msg);
        lengthInfo = makeError(StorageCode::kChecksumMismatch, std::move(msg));
      }
    }
  }

  XLOGF_IF(WARN, !lengthInfo, "Read job failed, result: {}, read io: {}, state: {}", lengthInfo, readIO_, state_);
  XLOGF(DBG7, "Read job completed, result: {}, read io: {}, state: {}", lengthInfo, readIO_, state_);

  result_.lengthInfo = std::move(lengthInfo);
  state_.chunkEngineJob.reset();
  batch_.finish(this);
}

BatchReadJob::BatchReadJob(std::span<const ReadIO> readIOs, std::span<IOResult> results, ChecksumType checksumType)
    : checksumType_(checksumType) {
  auto batchSize = readIOs.size();
  jobs_.reserve(batchSize);
  for (auto i = 0ul; i < batchSize; ++i) {
    jobs_.emplace_back(readIOs[i], results[i], *this);
  }
}

size_t BatchReadJob::addBufferToBatch(serde::CallContext::RDMATransmission &batch) {
  return addRangeToBatch(batch, 0, jobs_.size());
}

size_t BatchReadJob::addRangeToBatch(serde::CallContext::RDMATransmission &batch, size_t begin, size_t end) {
  batch.reserve(end - begin, end - begin);  // upper bound: coalescing may use fewer entries
  size_t writeCount = 0;
  size_t writeBytes = 0;
  for (auto i = begin; i < end; ++i) {
    auto &job = jobs_[i];
    if (job.result().lengthInfo) {
      auto length = *job.result().lengthInfo;
      auto localbuf = job.state().localbuf.subrange(job.state().headLength, length);
      auto result = batch.add(job.readIO().rdmabuf, localbuf);
      if (UNLIKELY(!result)) {
        rdmaWriteFails.addSample(1);
        job.result().lengthInfo = makeError(std::move(result.error()));
      } else {
        ++writeCount;
        writeBytes += length;
      }
    }
  }
  rdmaWriteCount.addSample(writeCount);
  rdmaWriteBytes.addSample(writeBytes);
  return writeBytes;
}

void BatchReadJob::initWaves(uint32_t waveSize) {
  waveSize_ = std::min<size_t>(waveSize, jobs_.size());
  if (waveSize_ == 0) {
    return;
  }
  auto numWaves = (jobs_.size() + waveSize_ - 1) / waveSize_;
  waves_ = std::vector<WaveState>(numWaves);
  for (auto i = 0ul; i < numWaves; ++i) {
    waves_[i].remaining.store(std::min<size_t>(waveSize_, jobs_.size() - i * waveSize_), std::memory_order_relaxed);
  }
}

size_t BatchReadJob::addWaveToBatch(uint32_t index, serde::CallContext::RDMATransmission &batch) {
  auto begin = size_t(index) * waveSize_;
  auto end = std::min(begin + waveSize_, jobs_.size());
  return addRangeToBatch(batch, begin, end);
}

void BatchReadJob::setWaveError(uint32_t index, const Status &error) {
  auto begin = size_t(index) * waveSize_;
  auto end = std::min(begin + waveSize_, jobs_.size());
  for (auto i = begin; i < end; ++i) {
    auto &job = jobs_[i];
    if (job.result().lengthInfo) {
      job.result().lengthInfo = makeError(error);
    }
  }
  anyPostFailed_.store(true, std::memory_order_relaxed);
}

size_t BatchReadJob::copyToRespBuffer(std::vector<uint8_t> &buffer) {
  // reserve the exact total once: the old per-job resize with a first-job-based
  // estimate caused repeated reallocations (and copies) for variable-sized jobs.
  size_t totalBytes = 0;
  for (auto &job : jobs_) {
    if (job.result().lengthInfo) {
      totalBytes += *job.result().lengthInfo;
    }
  }
  buffer.reserve(buffer.size() + totalBytes);

  size_t sendBytes = 0;
  for (auto &job : jobs_) {
    if (job.result().lengthInfo) {
      auto length = *job.result().lengthInfo;
      auto localbuf = job.state().localbuf.subrange(job.state().headLength, length);
      buffer.insert(buffer.end(), localbuf.ptr(), localbuf.ptr() + localbuf.size());
      sendBytes += length;
    }
  }
  return sendBytes;
}

void BatchReadJob::finish(AioReadJob *job) {
  if (!waves_.empty()) {
    auto waveIndex = static_cast<size_t>(job - jobs_.data()) / waveSize_;
    assert(waveIndex < waves_.size());
    if (waves_[waveIndex].remaining.fetch_sub(1, std::memory_order_acq_rel) == 1) {
      waves_[waveIndex].baton.post();
      anyWaveReady_.post();  // idempotent, first ready wave wins
    }
  }
  if (++finishedCount_ == jobs_.size()) {
    batchReadLatency.addSample(RelativeTime::now() - startTime());
    baton_.post();
  }
}

}  // namespace hf3fs::storage
