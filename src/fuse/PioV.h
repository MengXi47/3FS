#pragma once

#include <functional>

#include "client/meta/MetaClient.h"
#include "client/storage/StorageClient.h"
#include "common/utils/Result.h"

namespace hf3fs::lib::agent {
using flat::UserInfo;
class PioV {
 public:
  PioV(storage::client::StorageClient &storageClient, int chunkSizeLim, std::vector<ssize_t> &res);
  hf3fs::Result<Void> addRead(size_t idx,
                              const meta::Inode &inode,
                              uint16_t track,
                              off_t off,
                              size_t len,
                              void *buf,
                              storage::client::IOBuffer &memh);
  // if metaClient and userInfo are not nullptr,
  // meta server will be contacted for latest file length if known length is shorter than off
  CoTryTask<bool> checkWriteOff(size_t idx,
                                meta::client::MetaClient *metaClient,
                                const UserInfo *userInfo,
                                const meta::Inode &inode,
                                size_t off);
  hf3fs::Result<Void> addWrite(size_t idx,
                               const meta::Inode &inode,
                               uint16_t track,
                               off_t off,
                               size_t len,
                               const void *buf,
                               storage::client::IOBuffer &memh);
  CoTryTask<void> executeRead(const UserInfo &userInfo,
                              const storage::client::ReadOptions &options = storage::client::ReadOptions());
  CoTryTask<void> executeWrite(const UserInfo &userInfo,
                               const storage::client::WriteOptions &options = storage::client::WriteOptions());
  void finishIo(bool allowHoles);

 private:
  // templated on the chunk consumer: a std::function here would heap-allocate
  // for every user IO (the capture list exceeds the small-buffer size).
  template <typename ConsumeChunk>
  Result<Void> chunkIo(const meta::Inode &inode, uint16_t track, off_t off, size_t len, ConsumeChunk &&consumeChunk) {
    const auto &f = inode.asFile();
    auto chunkSize = f.layout.chunkSize;
    auto chunkOff = off % chunkSize;

    auto rcs = chunkSizeLim_ ? std::min((size_t)chunkSizeLim_, chunkSize.u64()) : chunkSize.u64();

    for (size_t lastL = 0, l = std::min((size_t)(chunkSize - chunkOff), len);  // l is within a chunk
         l < len + chunkSize;                                                  // for the last chunk
         lastL = l, l += chunkSize) {
      l = std::min(l, len);  // l is always growing longer
      auto opOff = off + lastL;

      auto chain = f.getChainId(inode, opOff, *routingInfo_, track);
      RETURN_ON_ERROR(chain);
      auto fchunk = f.getChunkId(inode.id, opOff);
      RETURN_ON_ERROR(fchunk);
      auto chunk = storage::ChunkId(*fchunk);
      auto chunkLen = l - lastL;

      for (size_t co = 0; co < chunkLen; co += rcs) {
        consumeChunk(*chain, chunk, chunkSize, chunkOff + co, std::min(rcs, chunkLen - co));
      }

      chunkOff = 0;  // chunks other than first always starts from 0
    }
    return Void{};
  }

 private:
  storage::client::StorageClient &storageClient_;
  int chunkSizeLim_;
  std::shared_ptr<flat::RoutingInfo> routingInfo_;
  std::vector<ssize_t> &res_;
  std::vector<storage::client::ReadIO> rios_;
  std::vector<storage::client::WriteIO> wios_;
  std::vector<storage::client::TruncateChunkOp> trops_;
  std::map<meta::InodeId, size_t> potentialLens_;
};
}  // namespace hf3fs::lib::agent
