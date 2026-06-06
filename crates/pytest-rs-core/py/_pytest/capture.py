from pytest._capture import CaptureBase as CaptureBase
from pytest._capture import CaptureFixture as CaptureFixture
from pytest._capture import CaptureIO as CaptureIO
from pytest._capture import CaptureManager as CaptureManager
from pytest._capture import CaptureResult as CaptureResult
from pytest._capture import DontReadFromInput as DontReadFromInput
from pytest._capture import EncodedFile as EncodedFile
from pytest._capture import FDCapture as FDCapture
from pytest._capture import FDCaptureBase as FDCaptureBase
from pytest._capture import FDCaptureBinary as FDCaptureBinary
from pytest._capture import MultiCapture as MultiCapture
from pytest._capture import NoCapture as NoCapture
from pytest._capture import SysCapture as SysCapture
from pytest._capture import SysCaptureBase as SysCaptureBase
from pytest._capture import SysCaptureBinary as SysCaptureBinary
from pytest._capture import TeeCaptureIO as TeeCaptureIO
from pytest._capture import _get_multicapture as _get_multicapture
from pytest._capture import patchsysdict as patchsysdict

from _pytest._stub import __getattr__  # noqa: E402, F401
