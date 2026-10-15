import { defineStrings } from "../../../lib/i18n/i18n";

/** An interrupted checkout or update: list badge, banner, checkout dialog and their actions. */
export const resumeStrings = defineStrings({
  en: {
    checkoutIncomplete: "Checkout incomplete — Continue checkout",
    updateIncomplete: "Update incomplete — Continue update",
    checkoutIncompleteTitle: "Checkout incomplete",
    updateIncompleteTitle: "Update incomplete",
    continueCheckout: "Continue checkout",
    continueUpdate: "Continue update",
    updateLatest: "Update to latest instead",
    checkoutIncompleteText:
      "The checkout stopped before it finished (connection lost, cancelled or the app was closed). Continue it at revision {rev}: what was already downloaded is kept.",
    updateIncompleteText:
      "An update stopped before it finished. Continue it at revision {rev}, or update to the latest revision instead.",
    incompleteHint:
      "Some folders were not downloaded yet. Continue to finish them at revision {rev}; nothing is deleted.",
    folderHasCheckout:
      "This folder already holds an unfinished checkout of this URL (revision {rev}). Continue it instead of starting over.",
    continueRequested: "Continuing…",
  },
  vi: {
    checkoutIncomplete: "Checkout chưa hoàn tất — Tiếp tục checkout",
    updateIncomplete: "Update chưa hoàn tất — Tiếp tục update",
    checkoutIncompleteTitle: "Checkout chưa hoàn tất",
    updateIncompleteTitle: "Update chưa hoàn tất",
    continueCheckout: "Tiếp tục checkout",
    continueUpdate: "Tiếp tục update",
    updateLatest: "Update lên bản mới nhất",
    checkoutIncompleteText:
      "Checkout đã dừng giữa chừng (mất kết nối, bị hủy hoặc ứng dụng bị đóng). Tiếp tục tại revision {rev}: phần đã tải về được giữ nguyên.",
    updateIncompleteText:
      "Update đã dừng giữa chừng. Tiếp tục tại revision {rev}, hoặc update lên revision mới nhất.",
    incompleteHint:
      "Một số thư mục chưa tải xong. Tiếp tục để hoàn tất tại revision {rev}; không xóa dữ liệu nào.",
    folderHasCheckout:
      "Thư mục này đang chứa một checkout chưa hoàn tất của URL này (revision {rev}). Hãy tiếp tục thay vì làm lại từ đầu.",
    continueRequested: "Đang tiếp tục…",
  },
});
