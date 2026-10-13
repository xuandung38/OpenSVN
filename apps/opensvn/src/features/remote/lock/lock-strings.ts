import { defineStrings } from "../../../lib/i18n/i18n";

export const lockStrings = defineStrings({
  en: {
    lockTitle: "Lock to edit",
    unlockTitle: "Unlock",
    files: "Files",
    comment: "Comment (optional)",
    lockAction: "Lock",
    unlockAction: "Unlock",
    stealAction: "Steal lock",
    lockedByOther: "Locked by {owner} since {when}.",
    lockComment: "Comment: {comment}",
    contactOwner: "Ask {owner} to unlock it, or steal the lock if you must edit now.",
    stealWarning:
      "Stealing the lock takes it from {owner}. Their unsaved edits may fail to commit.",
    stealConfirm: "I understand and want to steal this lock",
    breakLock: "Break the lock held by someone else (needs permission)",
    breakWarning: "Breaking a lock removes it from its owner.",
    alreadyYours: "You already hold this lock.",
    requested: "Request sent. Progress is in Activity.",
    errLockedByOther: "Another user holds this lock. You can steal it below.",
  },
  vi: {
    lockTitle: "Khóa để chỉnh sửa",
    unlockTitle: "Mở khóa",
    files: "File",
    comment: "Ghi chú (tùy chọn)",
    lockAction: "Khóa",
    unlockAction: "Mở khóa",
    stealAction: "Cướp khóa",
    lockedByOther: "{owner} đang khóa từ {when}.",
    lockComment: "Ghi chú: {comment}",
    contactOwner: "Hãy nhờ {owner} mở khóa, hoặc cướp khóa nếu cần sửa ngay.",
    stealWarning:
      "Cướp khóa sẽ lấy khóa khỏi {owner}. Thay đổi chưa lưu của họ có thể không commit được.",
    stealConfirm: "Tôi hiểu và muốn cướp khóa này",
    breakLock: "Phá khóa của người khác (cần quyền)",
    breakWarning: "Phá khóa sẽ gỡ khóa khỏi chủ sở hữu.",
    alreadyYours: "Bạn đã giữ khóa này.",
    requested: "Đã gửi yêu cầu. Tiến trình ở Activity.",
    errLockedByOther: "Người khác đang giữ khóa này. Bạn có thể cướp khóa bên dưới.",
  },
});
