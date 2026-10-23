// Thin barrel: the AppStore implementation lives in ./appStore/ (types,
// reducer, storage, AppProvider). All existing `from "../state/AppStore"`
// imports and vi.mock("../../state/AppStore") mocks keep working unchanged.
export {
  AppProvider,
  useApp,
  useAppActions,
  useAppState,
} from "./appStore/AppProvider";
export { toMessage } from "../shared/errors";
