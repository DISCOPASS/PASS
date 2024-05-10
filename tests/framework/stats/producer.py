# Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Producer of statistics."""

from abc import ABC, abstractmethod
from typing import Any, Callable

from framework import utils


# pylint: disable=R0903
class Producer(ABC):
    """Base class for raw results producer."""

    @abstractmethod
    def produce(self) -> Any:
        """Produce raw results."""


class SSHCommand(Producer):
    """Producer from executing ssh commands."""

    def __init__(self, cmd, ssh_connection):
        """Initialize the raw data producer."""
        self._cmd = cmd
        self._ssh_connection = ssh_connection

    def produce(self) -> Any:
        """Return the output of the executed ssh command."""
        rc, stdout, stderr = self._ssh_connection.execute_command(self._cmd)
        assert rc == 0
        assert stderr.read() == ""

        return stdout.read()


class HostCommand(Producer):
    """Producer from executing commands on host."""

    def __init__(self, cmd):
        """Initialize the raw data producer."""
        self._cmd = cmd

    def produce(self) -> Any:
        """Return output of the executed command."""
        result = utils.run_cmd(self._cmd)
        return result.stdout

    @property
    def cmd(self):
        """Return the command executed on host."""
        return self._cmd

    @cmd.setter
    def cmd(self, cmd):
        """Set the command executed on host."""
        self._cmd = cmd


class LambdaProducer(Producer):
    """Producer from calling python functions."""

    def __init__(self, func: Callable, func_kwargs=None):
        """Initialize the raw data producer."""
        super().__init__()
        assert callable(func)
        self._func = func
        self._func_kwargs = func_kwargs

    # pylint: disable=R1710
    def produce(self) -> Any:
        """Call `self._func`."""
        if self._func_kwargs:
            raw_data = self._func(**self._func_kwargs)
            return raw_data

        raw_data = self._func()
        return raw_data

    @property
    def func(self):
        """Return producer function."""
        return self._func

    @func.setter
    def func(self, func: Callable):
        self._func = func

    @property
    def func_kwargs(self):
        """Return producer function arguments."""
        return self._func_kwargs

    @func_kwargs.setter
    def func_kwargs(self, func_kwargs):
        self._func_kwargs = func_kwargs
