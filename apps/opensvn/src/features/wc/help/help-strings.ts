import { defineStrings } from "../../../lib/i18n/i18n";

export const helpStrings = defineStrings({
  en: {
    reportTitle: "Report a problem",
    reportIntro:
      "This builds a bundle of logs and settings to attach to a bug report. Passwords and tokens are always removed. Review every file below; nothing is sent anywhere, you choose where to save the zip.",
    reportOptions: "What to hide",
    reportHidePaths: "Hide folder paths",
    reportHideMessages: "Hide commit messages",
    reportBuilding: "Building the bundle…",
    reportFiles: "Files in the bundle",
    reportContent: "Content of {name}",
    reportSave: "Save as zip…",
    reportSaved: "Saved diagnostics to {path}",
    cancel: "Cancel",
    retry: "Retry",
  },
  vi: {
    reportTitle: "Báo lỗi",
    reportIntro:
      "Tạo gói log và cấu hình để đính kèm vào báo lỗi. Mật khẩu và token luôn bị gỡ bỏ. Hãy xem kỹ từng file bên dưới; app không gửi đi đâu cả, bạn tự chọn nơi lưu file zip.",
    reportOptions: "Thông tin cần ẩn",
    reportHidePaths: "Ẩn đường dẫn thư mục",
    reportHideMessages: "Ẩn commit message",
    reportBuilding: "Đang tạo gói…",
    reportFiles: "Các file trong gói",
    reportContent: "Nội dung {name}",
    reportSave: "Lưu thành zip…",
    reportSaved: "Đã lưu gói chẩn đoán vào {path}",
    cancel: "Hủy",
    retry: "Thử lại",
  },
});
